use crate::api::{self, js_json};
use crate::common::{Code, Ctx};
use leptos::prelude::*;
use leptos::task::spawn_local;
use rustle_types::*;

#[component]
pub fn Wizard() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let step = RwSignal::new(1u8);
    let name = RwSignal::new(String::new());
    let domain = RwSignal::new(String::new());
    let repo = RwSignal::new(String::new());
    let registry = RwSignal::new(String::new());
    let ram = RwSignal::new(256u32);
    let port = RwSignal::new("3000".to_string());
    let env = RwSignal::new(Vec::<(RwSignal<String>, RwSignal<String>)>::new());
    let skip_dns = RwSignal::new(false);
    let over_budget = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let result = RwSignal::new(None::<(SiteDto, String, WorkflowResp)>);
    let registries = LocalResource::new(move || async move {
        api::get::<Vec<RegistryDto>>("/api/v1/registries")
            .await
            .unwrap_or_default()
    });

    let close = move || {
        ctx.wizard.set(false);
        ctx.refresh();
    };

    let valid1 = move || {
        let n = name.get();
        !n.is_empty()
            && !domain.get().trim().is_empty()
            && repo.get().trim().starts_with("ghcr.io/")
            && n.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };

    let create = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let env_vars: Vec<EnvVar> = env
                .get_untracked()
                .iter()
                .filter(|(k, _)| !k.get_untracked().trim().is_empty())
                .map(|(k, v)| EnvVar {
                    key: k.get_untracked().trim().to_string(),
                    value: v.get_untracked(),
                })
                .collect();
            let req = CreateSiteReq {
                name: name.get_untracked(),
                domain: domain.get_untracked().trim().to_string(),
                image_repo: repo.get_untracked().trim().to_string(),
                ram_limit_mb: ram.get_untracked(),
                container_port: port.get_untracked().parse().unwrap_or(3000),
                registry_id: Some(registry.get_untracked()).filter(|r| !r.is_empty()),
                env: env_vars,
                skip_dns_check: skip_dns.get_untracked(),
                override_budget: over_budget.get_untracked(),
            };
            match api::call::<SiteDto>("POST", "/api/v1/sites", js_json(&req)).await {
                Err(e) => error.set(Some(match e.code.as_str() {
                    "dns_missing" | "dns_mismatch" => {
                        format!("{} Tick “Skip DNS check” to continue anyway.", e.message)
                    }
                    "budget_exceeded" => format!(
                        "{} Tick “Allow exceeding the RAM budget” to continue anyway.",
                        e.message
                    ),
                    _ => e.message,
                })),
                Ok(site) => {
                    let tok = api::call::<TokenCreated>(
                        "POST",
                        &format!("/api/v1/sites/{}/tokens", site.id),
                        serde_json::json!({}),
                    )
                    .await;
                    let wf =
                        api::get::<WorkflowResp>(&format!("/api/v1/sites/{}/workflow", site.id))
                            .await;
                    match (tok, wf) {
                        (Ok(t), Ok(w)) => {
                            result.set(Some((site, t.token, w)));
                            step.set(4);
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            error.set(Some(format!("Site created, but: {}", e.message)));
                        }
                    }
                }
            }
            busy.set(false);
        });
    };

    let indexed = move || env.get().into_iter().enumerate().collect::<Vec<_>>();
    view! {
        <div class="modal-bg">
            <div class="modal">
                <div class="modal-head">
                    <h3>"Add site"</h3>
                    <span class="muted">{move || format!("Step {} of 4", step.get())}</span>
                    <button class="ghost" on:click=move |_| close()>"✕"</button>
                </div>
                <div class="steps">
                    {["Basics", "Resources", "Review", "Deploy setup"].into_iter().enumerate().map(|(i, l)| { let on = move || (step.get() as usize) > i; view! { <span class:on=on>{format!("{}. {}", i + 1, l)}</span> } }).collect_view()}
                </div>
                <div class="modal-body">
                    <Show when=move || step.get() == 1>
                        <div class="form">
                            <label>"Name (lowercase letters, digits, dashes)"</label>
                            <input placeholder="my-blog" prop:value=move || name.get() on:input=move |e| name.set(event_target_value(&e))/>
                            <label>"Domain (its A record must point to this server)"</label>
                            <input placeholder="blog.example.com" prop:value=move || domain.get() on:input=move |e| domain.set(event_target_value(&e))/>
                            <label>"Image repository on GHCR"</label>
                            <input class="mono" placeholder="ghcr.io/owner/app" prop:value=move || repo.get() on:input=move |e| repo.set(event_target_value(&e))/>
                            <label>"Registry login (only for private images)"</label>
                            <select on:change=move |e| registry.set(event_target_value(&e))>
                                <option value="">"Public image"</option>
                                {move || registries.get().unwrap_or_default().into_iter().map(|r| view! { <option value=r.id.clone()>{format!("{} ({})", r.name, r.username)}</option> }).collect_view()}
                            </select>
                            <p class="muted small">"Add a GHCR token under Settings → Registries for private images."</p>
                        </div>
                    </Show>
                    <Show when=move || step.get() == 2>
                        <div class="form">
                            <label>{move || format!("RAM limit (hard ceiling): {} MB", ram.get())}</label>
                            <input type="range" min="64" max="512" step="32" prop:value=move || ram.get().to_string() on:input=move |e| ram.set(event_target_value(&e).parse().unwrap_or(256))/>
                            <div class="muted small spread"><span>"64 static"</span><span>"256 typical Next.js"</span><span>"512 heavy"</span></div>
                            <label>"Container port"</label>
                            <input type="number" min="1" max="65535" prop:value=move || port.get() on:input=move |e| port.set(event_target_value(&e))/>
                            <label>"Environment variables"</label>
                            <For each=indexed key=|(i, _)| *i children=move |(i, (k, v))| view! {
                                <div class="env-row">
                                    <input class="mono" placeholder="KEY" prop:value=move || k.get() on:input=move |e| k.set(event_target_value(&e))/>
                                    <input class="mono" placeholder="value" prop:value=move || v.get() on:input=move |e| v.set(event_target_value(&e))/>
                                    <button class="ghost sm" on:click=move |_| env.update(|r| { if i < r.len() { r.remove(i); } })>"✕"</button>
                                </div>
                            }/>
                            <button class="sm" on:click=move |_| env.update(|r| r.push((RwSignal::new(String::new()), RwSignal::new(String::new()))))>"+ Add variable"</button>
                        </div>
                    </Show>
                    <Show when=move || step.get() == 3>
                        <div class="form">
                            <dl>
                                <dt>"Site"</dt><dd>{move || name.get()}</dd>
                                <dt>"Address"</dt><dd class="mono">{move || format!("https://{}", domain.get())}</dd>
                                <dt>"Image"</dt><dd class="mono">{move || repo.get()}</dd>
                                <dt>"Resources"</dt><dd>{move || format!("{} MB RAM, port {}", ram.get(), port.get())}</dd>
                            </dl>
                            <label class="check"><input type="checkbox" prop:checked=move || skip_dns.get() on:change=move |e| skip_dns.set(event_target_checked(&e))/>" Skip DNS check"</label>
                            <label class="check"><input type="checkbox" prop:checked=move || over_budget.get() on:change=move |e| over_budget.set(event_target_checked(&e))/>" Allow exceeding the RAM budget"</label>
                            <Show when=move || error.get().is_some()><div class="banner bad">{move || error.get().unwrap_or_default()}</div></Show>
                        </div>
                    </Show>
                    <Show when=move || step.get() == 4>
                        {move || result.get().map(|(site, token, wf)| view! {
                            <div class="form">
                                <div class="banner good">{format!("Site “{}” created. It goes live after its first deploy; HTTPS is issued automatically.", site.name)}</div>
                                <p>"Add these two secrets to your GitHub repository (Settings → Secrets and variables → Actions):"</p>
                                <div class="code"><div class="code-head"><span>"RUSTLE_TOKEN (shown once)"</span><button class="ghost sm" on:click={let t = token.clone(); move |_| { crate::common::copy_text(&t); ctx.ok("Copied"); }}>"Copy"</button></div><pre>{token.clone()}</pre></div>
                                <p class="muted small">"RUSTLE_URL = the address you use to open this dashboard, e.g. https://admin.example.com"</p>
                                <Code title=".github/workflows/deploy.yml" text=wf.workflow_yaml/>
                                <Code title="Dockerfile (optional, Next.js standalone)" text=wf.dockerfile/>
                            </div>
                        })}
                    </Show>
                </div>
                <div class="modal-foot">
                    <Show when=move || { let s = step.get(); s > 1 && s < 4 }><button on:click=move |_| step.update(|s| *s -= 1)>"Back"</button></Show>
                    <div class="grow"></div>
                    <Show when=move || step.get() == 1><button class="primary" disabled=move || !valid1() on:click=move |_| step.set(2)>"Next"</button></Show>
                    <Show when=move || step.get() == 2><button class="primary" on:click=move |_| step.set(3)>"Next"</button></Show>
                    <Show when=move || step.get() == 3><button class="primary" disabled=move || busy.get() on:click=create>{move || if busy.get() { "Creating…" } else { "Create site" }}</button></Show>
                    <Show when=move || step.get() == 4><button class="primary" on:click=move |_| close()>"Done"</button></Show>
                </div>
            </div>
        </div>
    }
}
