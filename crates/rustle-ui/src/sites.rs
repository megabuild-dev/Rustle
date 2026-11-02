use crate::api::{self, js_json};
use crate::common::{confirm, digest_short, fmt_ts, Badge, Code, Ctx};
use crate::overview::SiteRow;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rustle_types::*;
use std::time::Duration;

#[component]
pub fn SiteList() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let sites = LocalResource::new(move || {
        ctx.tick.get();
        async move { api::get::<Vec<SiteDto>>("/api/v1/sites").await }
    });
    view! {
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || sites.get().map(|r| match r {
                Ok(list) if list.is_empty() => view! { <p class="muted">"No sites yet."</p> }.into_any(),
                Ok(list) => view! { <div class="list">{list.into_iter().map(|s| view! { <SiteRow s=s/> }).collect_view()}</div> }.into_any(),
                Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
            })}
        </Suspense>
    }
}

const TABS: [(&str, &str); 6] = [
    ("overview", "Overview"),
    ("deploys", "Deploys"),
    ("env", "Environment"),
    ("tokens", "Deploy tokens"),
    ("logs", "Logs"),
    ("ci", "CI/CD"),
];

#[component]
pub fn SiteDetail(id: String, tab: String) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let id2 = id.clone();
    let site = LocalResource::new(move || {
        ctx.tick.get();
        let id = id2.clone();
        async move { api::get::<SiteDto>(&format!("/api/v1/sites/{id}")).await }
    });
    view! {
        <p><a href="#/sites" class="muted">"← All sites"</a></p>
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || {
                let tab = tab.clone();
                site.get().map(move |r| match r {
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                    Ok(s) => {
                        let body = match tab.as_str() {
                            "deploys" => view! { <Deploys site=s.clone()/> }.into_any(),
                            "env" => view! { <EnvTab site=s.clone()/> }.into_any(),
                            "tokens" => view! { <Tokens site=s.clone()/> }.into_any(),
                            "logs" => view! { <Logs site=s.clone()/> }.into_any(),
                            "ci" => view! { <Ci site=s.clone()/> }.into_any(),
                            _ => view! { <Overview s=s.clone()/> }.into_any(),
                        };
                        view! {
                            <div class="title">
                                <h3>{s.name.clone()} " " <Badge status=s.status.clone()/></h3>
                                <a class="mono muted" href=format!("https://{}", s.domain) target="_blank" rel="noreferrer">{format!("https://{}", s.domain)}</a>
                            </div>
                            {body}
                        }.into_any()
                    }
                })
            }}
        </Suspense>
    }
}

#[component]
fn Tabs(id: String, active: String) -> impl IntoView {
    view! {
        <div class="tabs">
            {TABS.iter().map(|(k, label)| {
                let is = *k == active;
                view! { <a href=format!("#/sites/{id}/{k}") class:active=is>{*label}</a> }
            }).collect_view()}
        </div>
    }
}

async fn act(ctx: Ctx, id: String, what: &'static str) {
    match api::call::<SiteDto>(
        "POST",
        &format!("/api/v1/sites/{id}/{what}"),
        serde_json::json!({}),
    )
    .await
    {
        Ok(_) => {
            ctx.ok(&format!("{what} ok"));
            ctx.refresh();
        }
        Err(e) => ctx.err(&e),
    }
}

#[component]
fn Overview(s: SiteDto) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let ram = RwSignal::new(s.ram_limit_mb);
    let port = RwSignal::new(s.container_port.to_string());
    let zero = RwSignal::new(s.zero_downtime);
    let compat = RwSignal::new(s.compat_caps);
    let id = s.id.clone();
    let id_save = id.clone();
    let save = move |_| {
        let id = id_save.clone();
        spawn_local(async move {
            let req = UpdateSiteReq {
                ram_limit_mb: Some(ram.get_untracked()),
                container_port: port.get_untracked().parse().ok(),
                zero_downtime: Some(zero.get_untracked()),
                compat_caps: Some(compat.get_untracked()),
                ..Default::default()
            };
            match api::call::<SiteDto>("PATCH", &format!("/api/v1/sites/{id}"), js_json(&req)).await
            {
                Ok(_) => {
                    ctx.ok("Saved. Redeploy to apply RAM, port or capability changes.");
                    ctx.refresh();
                }
                Err(e) => ctx.err(&e),
            }
        });
    };
    let (a1, a2, a3, a4, del) = (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    let name = s.name.clone();
    view! {
        <Tabs id=s.id.clone() active="overview".to_string()/>
        <div class="panel">
            <div class="actions">
                <button on:click=move |_| { let i = a1.clone(); spawn_local(act(ctx, i, "restart")) }>"Restart"</button>
                <button on:click=move |_| { let i = a2.clone(); spawn_local(act(ctx, i, "stop")) }>"Stop"</button>
                <button on:click=move |_| { let i = a3.clone(); spawn_local(act(ctx, i, "start")) }>"Start"</button>
                <button on:click=move |_| {
                    let i = a4.clone();
                    spawn_local(async move {
                        match api::call::<DeployAccepted>("POST", &format!("/api/v1/sites/{i}/redeploy"), serde_json::json!({})).await {
                            Ok(_) => { ctx.ok("Redeploy started"); ctx.refresh(); }
                            Err(e) => ctx.err(&e),
                        }
                    })
                }>"Redeploy"</button>
                <button class="danger" on:click=move |_| {
                    if !confirm(&format!("Delete site “{name}”? Its container and route are removed.")) { return; }
                    let i = del.clone();
                    spawn_local(async move {
                        match api::call_unit("DELETE", &format!("/api/v1/sites/{i}"), None).await {
                            Ok(()) => { ctx.ok("Site deleted"); if let Some(w) = web_sys::window() { let _ = w.location().set_hash("#/sites"); } }
                            Err(e) => ctx.err(&e),
                        }
                    })
                }>"Delete"</button>
            </div>
            <dl>
                <dt>"Image repository"</dt><dd class="mono">{s.image_repo.clone()}</dd>
                <dt>"Last deploy"</dt><dd>{s.last_deployed_at.map(fmt_ts).unwrap_or("-".into())}</dd>
                <dt>"Memory now"</dt><dd>{s.ram_used_mb.map(|v| format!("{v} MB")).unwrap_or("-".into())}</dd>
            </dl>
            <h4>"Resources and behaviour"</h4>
            <div class="form">
                <label>"RAM limit (hard)"</label>
                <select on:change=move |e| ram.set(event_target_value(&e).parse().unwrap_or(256))>
                    {(64..=512).step_by(32).map(|v| view! { <option value=v.to_string() selected=move || ram.get() == v>{format!("{v} MB")}</option> }).collect_view()}
                </select>
                <label>"Container port"</label>
                <input type="number" min="1" max="65535" prop:value=move || port.get() on:input=move |e| port.set(event_target_value(&e))/>
                <label class="check"><input type="checkbox" prop:checked=move || zero.get() on:change=move |e| zero.set(event_target_checked(&e))/>" Zero-downtime deploys (starts the new version first; needs spare RAM)"</label>
                <label class="check"><input type="checkbox" prop:checked=move || compat.get() on:change=move |e| compat.set(event_target_checked(&e))/>" Compatibility capabilities (for images that switch user, e.g. nginx)"</label>
                <button class="primary" on:click=save>"Save"</button>
            </div>
        </div>
    }
}

#[component]
fn Deploys(site: SiteDto) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let id = site.id.clone();
    let current = site.current_deployment_id.clone();
    let list = LocalResource::new(move || {
        ctx.tick.get();
        let id = id.clone();
        async move { api::get::<Vec<DeploymentDto>>(&format!("/api/v1/sites/{id}/deployments")).await }
    });
    // While something is in flight, keep refreshing.
    let handle = set_interval_with_handle(move || ctx.refresh(), Duration::from_secs(4)).ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    let sid = site.id.clone();
    view! {
        <Tabs id=site.id.clone() active="deploys".to_string()/>
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || {
                let current = current.clone();
                let sid = sid.clone();
                list.get().map(move |r| match r {
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <div class="panel"><p class="muted">"No deployments yet. Push to your repository or use the CI/CD tab."</p></div> }.into_any(),
                    Ok(v) => view! {
                        <table>
                            <thead><tr><th>"When"</th><th>"Status"</th><th>"Trigger"</th><th>"Image"</th><th></th></tr></thead>
                            <tbody>{v.into_iter().map(|d| {
                                let can = matches!(d.status.as_str(), "running" | "superseded") && Some(&d.id) != current.as_ref();
                                let (sid, did) = (sid.clone(), d.id.clone());
                                view! {
                                    <tr>
                                        <td>{fmt_ts(d.started_at)}</td>
                                        <td><Badge status=d.status.clone()/>{d.error.clone().map(|e| view! { <div class="err small">{e}</div> })}</td>
                                        <td>{d.trigger.clone()}</td>
                                        <td class="mono">{digest_short(&d.image_digest)}</td>
                                        <td>{can.then(|| view! {
                                            <button class="sm" on:click=move |_| {
                                                if !confirm("Roll back to this deployment (image and environment)?") { return; }
                                                let (sid, did) = (sid.clone(), did.clone());
                                                spawn_local(async move {
                                                    match api::call::<DeployAccepted>("POST", &format!("/api/v1/sites/{sid}/rollback"), serde_json::json!({"deployment_id": did})).await {
                                                        Ok(_) => { ctx.ok("Rollback started"); ctx.refresh(); }
                                                        Err(e) => ctx.err(&e),
                                                    }
                                                })
                                            }>"Roll back"</button>
                                        })}</td>
                                    </tr>
                                }
                            }).collect_view()}</tbody>
                        </table>
                    }.into_any(),
                })
            }}
        </Suspense>
    }
}

#[component]
fn EnvTab(site: SiteDto) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let rows = RwSignal::new(Vec::<(RwSignal<String>, RwSignal<String>)>::new());
    let masked = RwSignal::new(true);
    let id = site.id.clone();
    let load = {
        let id = id.clone();
        move |reveal: bool| {
            let id = id.clone();
            spawn_local(async move {
                match api::get::<EnvListResp>(&format!("/api/v1/sites/{id}/env?reveal={reveal}"))
                    .await
                {
                    Ok(r) => {
                        masked.set(r.masked);
                        rows.set(
                            r.vars
                                .into_iter()
                                .map(|v| (RwSignal::new(v.key), RwSignal::new(v.value)))
                                .collect(),
                        );
                    }
                    Err(e) => ctx.err(&e),
                }
            })
        }
    };
    load(false);
    let load_reveal = load.clone();
    let reveal_cb = Callback::new(move |()| load_reveal(true));
    let id_save = id.clone();
    let save = move |_| {
        let id = id_save.clone();
        let vars: Vec<EnvVar> = rows
            .get_untracked()
            .iter()
            .filter(|(k, _)| !k.get_untracked().trim().is_empty())
            .map(|(k, v)| EnvVar {
                key: k.get_untracked().trim().to_string(),
                value: v.get_untracked(),
            })
            .collect();
        spawn_local(async move {
            match api::call_unit(
                "PUT",
                &format!("/api/v1/sites/{id}/env"),
                Some(js_json(&PutEnvReq { vars })),
            )
            .await
            {
                Ok(()) => ctx.ok("Saved. Redeploy to apply."),
                Err(e) => ctx.err(&e),
            }
        });
    };
    let id_re = id.clone();
    let indexed = move || rows.get().into_iter().enumerate().collect::<Vec<_>>();
    view! {
        <Tabs id=site.id.clone() active="env".to_string()/>
        <div class="panel">
            <p class="muted">"Values are encrypted at rest and hidden by default. Changes apply on the next deploy."</p>
            <div class="env">
                <For each=indexed key=|(i, _)| *i children=move |(i, (k, v))| view! {
                    <div class="env-row">
                        <input class="mono" placeholder="KEY" prop:value=move || k.get() on:input=move |e| k.set(event_target_value(&e))/>
                        <input class="mono" placeholder="value" prop:value=move || v.get() on:input=move |e| v.set(event_target_value(&e))/>
                        <button class="ghost sm" on:click=move |_| rows.update(|r| { if i < r.len() { r.remove(i); } })>"✕"</button>
                    </div>
                }/>
            </div>
            <div class="actions">
                <button on:click=move |_| rows.update(|r| r.push((RwSignal::new(String::new()), RwSignal::new(String::new()))))>"+ Add variable"</button>
                <Show when=move || masked.get()><button on:click=move |_| reveal_cb.run(())>"Reveal values"</button></Show>
                <button class="primary" on:click=save>"Save"</button>
                <button on:click=move |_| {
                    let id = id_re.clone();
                    spawn_local(async move {
                        match api::call::<DeployAccepted>("POST", &format!("/api/v1/sites/{id}/redeploy"), serde_json::json!({})).await {
                            Ok(_) => ctx.ok("Redeploy started"),
                            Err(e) => ctx.err(&e),
                        }
                    })
                }>"Redeploy now"</button>
            </div>
        </div>
    }
}

#[component]
fn Tokens(site: SiteDto) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let id = site.id.clone();
    let created = RwSignal::new(None::<String>);
    let list = LocalResource::new({
        let id = id.clone();
        move || {
            ctx.tick.get();
            let id = id.clone();
            async move { api::get::<Vec<TokenDto>>(&format!("/api/v1/sites/{id}/tokens")).await }
        }
    });
    let id_new = id.clone();
    let create = move |_| {
        let id = id_new.clone();
        spawn_local(async move {
            match api::call::<TokenCreated>(
                "POST",
                &format!("/api/v1/sites/{id}/tokens"),
                serde_json::json!({}),
            )
            .await
            {
                Ok(t) => {
                    created.set(Some(t.token));
                    ctx.refresh();
                }
                Err(e) => ctx.err(&e),
            }
        });
    };
    let sid = id.clone();
    view! {
        <Tabs id=site.id.clone() active="tokens".to_string()/>
        <div class="panel">
            <p class="muted">"A deploy token lets GitHub Actions deploy THIS site only. It is shown once; store it as the repository secret RUSTLE_TOKEN."</p>
            <Show when=move || created.get().is_some()>
                <div class="banner good">
                    <div>"New token (copy it now, it will not be shown again):"</div>
                    <div class="mono wrap">{move || created.get().unwrap_or_default()}</div>
                    <button class="sm" on:click=move |_| { crate::common::copy_text(&created.get_untracked().unwrap_or_default()); ctx.ok("Copied"); }>"Copy"</button>
                </div>
            </Show>
            <button class="primary" on:click=create>"Create token"</button>
        </div>
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || {
                let sid = sid.clone();
                list.get().map(move |r| match r {
                    Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                    Ok(v) => view! {
                        <table>
                            <thead><tr><th>"Token"</th><th>"Created"</th><th>"Last used"</th><th></th></tr></thead>
                            <tbody>{v.into_iter().map(|t| {
                                let (sid, tid) = (sid.clone(), t.id.clone());
                                let active = t.revoked_at.is_none();
                                view! { <tr>
                                    <td class="mono">{format!("{}…", t.prefix)}{(!active).then(|| view! { " " <Badge status="revoked"/> })}</td>
                                    <td>{fmt_ts(t.created_at)}</td>
                                    <td>{t.last_used_at.map(fmt_ts).unwrap_or("never".into())}</td>
                                    <td>{active.then(|| view! { <button class="danger sm" on:click=move |_| {
                                        if !confirm("Revoke this token? Deploys using it will stop working.") { return; }
                                        let (sid, tid) = (sid.clone(), tid.clone());
                                        spawn_local(async move {
                                            match api::call_unit("DELETE", &format!("/api/v1/sites/{sid}/tokens/{tid}"), None).await {
                                                Ok(()) => { ctx.ok("Token revoked"); ctx.refresh(); }
                                                Err(e) => ctx.err(&e),
                                            }
                                        })
                                    }>"Revoke"</button> })}</td>
                                </tr> }
                            }).collect_view()}</tbody>
                        </table>
                    }.into_any(),
                })
            }}
        </Suspense>
    }
}

#[component]
fn Logs(site: SiteDto) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let text = RwSignal::new(String::new());
    let follow = RwSignal::new(true);
    let id = site.id.clone();
    let fetch = move || {
        let id = id.clone();
        spawn_local(async move {
            match api::get::<serde_json::Value>(&format!("/api/v1/sites/{id}/logs?tail=300")).await
            {
                Ok(v) => text.set(v["logs"].as_str().unwrap_or("").to_string()),
                Err(e) if e.status == 409 => text.set("No running container yet.".into()),
                Err(e) => ctx.err(&e),
            }
        })
    };
    fetch.clone()();
    let f2 = fetch.clone();
    let handle = set_interval_with_handle(
        move || {
            if follow.get_untracked() {
                f2.clone()()
            }
        },
        Duration::from_secs(3),
    )
    .ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    view! {
        <Tabs id=site.id.clone() active="logs".to_string()/>
        <div class="actions">
            <label class="check"><input type="checkbox" prop:checked=move || follow.get() on:change=move |e| follow.set(event_target_checked(&e))/>" Follow"</label>
            <button on:click=move |_| fetch.clone()()>"Refresh"</button>
        </div>
        <pre class="logs">{move || text.get()}</pre>
    }
}

#[component]
fn Ci(site: SiteDto) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let data = LocalResource::new({
        let id = site.id.clone();
        move || {
            let id = id.clone();
            async move { api::get::<WorkflowResp>(&format!("/api/v1/sites/{id}/workflow")).await }
        }
    });
    let _ = ctx;
    view! {
        <Tabs id=site.id.clone() active="ci".to_string()/>
        <div class="panel">
            <p>"1. Create a deploy token (Deploy tokens tab) and add two repository secrets in GitHub: "<code>"RUSTLE_URL"</code>" (your dashboard URL) and "<code>"RUSTLE_TOKEN"</code>"."</p>
            <p>"2. Commit the workflow below as "<code>".github/workflows/deploy.yml"</code>". Builds run on GitHub, never on this server."</p>
            <p>"3. Optional: a small Next.js image (set "<code>"output: \"standalone\""</code>" in next.config)."</p>
        </div>
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || data.get().map(|r| match r {
                Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                Ok(w) => view! {
                    <Code title=".github/workflows/deploy.yml" text=w.workflow_yaml/>
                    <Code title="Dockerfile (Next.js with a Node server: output standalone)" text=w.dockerfile/>
                    <Code title="Dockerfile (Next.js static export: output export, served by Caddy, about 64 MB RAM)" text=w.dockerfile_static/>
                }.into_any(),
            })}
        </Suspense>
    }
}
