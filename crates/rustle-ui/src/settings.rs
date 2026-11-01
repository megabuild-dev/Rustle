use crate::api::{self, js_json};
use crate::common::{confirm, fmt_ts, Ctx};
use leptos::prelude::*;
use leptos::task::spawn_local;
use rustle_types::*;

#[component]
pub fn SettingsPage() -> impl IntoView {
    view! {
        <Account/>
        <Dashboard/>
        <Registries/>
        <Backup/>
        <Updates/>
        <Maintenance/>
    }
}

#[component]
fn Account() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let (cur, new1, new2) = (
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
    );
    let save = move |_| {
        if new1.get_untracked() != new2.get_untracked() {
            ctx.toast
                .set(Some((false, "The new passwords do not match".into())));
            return;
        }
        spawn_local(async move {
            let body = js_json(&ChangePasswordReq {
                current: cur.get_untracked(),
                new: new1.get_untracked(),
            });
            match api::call_unit("POST", "/api/v1/auth/password", Some(body)).await {
                Ok(()) => {
                    ctx.user.set(None); // every session was revoked
                }
                Err(e) => ctx.err(&e),
            }
        });
    };
    view! {
        <div class="panel"><h4>"Administrator password"</h4>
            <div class="form">
                <label>"Current password"</label><input type="password" prop:value=move || cur.get() on:input=move |e| cur.set(event_target_value(&e))/>
                <label>"New password (12+ characters)"</label><input type="password" prop:value=move || new1.get() on:input=move |e| new1.set(event_target_value(&e))/>
                <label>"Repeat new password"</label><input type="password" prop:value=move || new2.get() on:input=move |e| new2.set(event_target_value(&e))/>
                <button class="primary" on:click=save>"Change password"</button>
                <p class="muted small">"You will be signed out everywhere."</p>
            </div>
        </div>
    }
}

#[component]
fn Dashboard() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let (domain, email, budget) = (
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
    );
    let skip_dns = RwSignal::new(false);
    spawn_local(async move {
        if let Ok(s) = api::get::<SettingsDto>("/api/v1/system/settings").await {
            domain.set(s.admin_domain);
            email.set(s.acme_email);
            budget.set(
                s.budget_override_mb
                    .map(|b| b.to_string())
                    .unwrap_or_default(),
            );
        }
    });
    let save = move |_| {
        spawn_local(async move {
            let req = SettingsDto {
                admin_domain: domain.get_untracked(),
                acme_email: email.get_untracked(),
                budget_override_mb: budget.get_untracked().trim().parse().ok(),
                skip_dns_check: skip_dns.get_untracked(),
            };
            match api::call::<SettingsDto>("PUT", "/api/v1/system/settings", js_json(&req)).await {
                Ok(_) => ctx.ok("Saved. A certificate is requested for the new domain; point its A record at this server first."),
                Err(e) => ctx.err(&e),
            }
        });
    };
    view! {
        <div class="panel"><h4>"Dashboard address and RAM budget"</h4>
            <div class="form">
                <label>"Dashboard domain (empty = HTTPS on the server IP with a self-signed certificate)"</label>
                <input placeholder="admin.example.com" prop:value=move || domain.get() on:input=move |e| domain.set(event_target_value(&e))/>
                <label class="check"><input type="checkbox" prop:checked=move || skip_dns.get() on:change=move |e| skip_dns.set(event_target_checked(&e))/>" Skip the DNS check (only if you know the domain will point here)"</label>
                <label>"Email for Let's Encrypt notices"</label>
                <input placeholder="you@example.com" prop:value=move || email.get() on:input=move |e| email.set(event_target_value(&e))/>
                <label>"RAM budget override in MB (empty = automatic)"</label>
                <input type="number" prop:value=move || budget.get() on:input=move |e| budget.set(event_target_value(&e))/>
                <button class="primary" on:click=save>"Save"</button>
            </div>
        </div>
    }
}

#[component]
fn Registries() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let list = LocalResource::new(move || {
        ctx.tick.get();
        async move {
            api::get::<Vec<RegistryDto>>("/api/v1/registries")
                .await
                .unwrap_or_default()
        }
    });
    let (name, user, secret) = (
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
    );
    let add = move |_| {
        spawn_local(async move {
            let req = CreateRegistryReq {
                name: name.get_untracked(),
                username: user.get_untracked(),
                secret: secret.get_untracked(),
            };
            match api::call::<RegistryDto>("POST", "/api/v1/registries", js_json(&req)).await {
                Ok(_) => {
                    secret.set(String::new());
                    ctx.ok("Registry login saved");
                    ctx.refresh();
                }
                Err(e) => ctx.err(&e),
            }
        });
    };
    view! {
        <div class="panel"><h4>"Registry logins (private GHCR images)"</h4>
            <p class="muted">"Use a GitHub personal access token with the read:packages scope. It is stored encrypted and only used when pulling."</p>
            <Suspense fallback=|| ()>{move || list.get().map(|v| v.into_iter().map(|r| {
                let id = r.id.clone();
                view! { <div class="env-row"><span>{format!("{} ({})", r.name, r.username)}</span><button class="danger sm" on:click=move |_| {
                    if !confirm("Remove this registry login?") { return; }
                    let id = id.clone();
                    spawn_local(async move { match api::call_unit("DELETE", &format!("/api/v1/registries/{id}"), None).await { Ok(()) => ctx.refresh(), Err(e) => ctx.err(&e) } })
                }>"Remove"</button></div> }
            }).collect_view())}</Suspense>
            <div class="form">
                <label>"Name"</label><input prop:value=move || name.get() on:input=move |e| name.set(event_target_value(&e))/>
                <label>"GitHub username"</label><input prop:value=move || user.get() on:input=move |e| user.set(event_target_value(&e))/>
                <label>"Token"</label><input type="password" prop:value=move || secret.get() on:input=move |e| secret.set(event_target_value(&e))/>
                <button class="primary" on:click=add>"Add"</button>
            </div>
        </div>
    }
}

#[component]
fn Backup() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let (enabled, repo, branch, prefix, token) = (
        RwSignal::new(false),
        RwSignal::new(String::new()),
        RwSignal::new("main".to_string()),
        RwSignal::new("rustle-backups".to_string()),
        RwSignal::new(String::new()),
    );
    let token_set = RwSignal::new(false);
    let runs = LocalResource::new(move || {
        ctx.tick.get();
        async move {
            api::get::<Vec<BackupRunDto>>("/api/v1/system/backup/runs")
                .await
                .unwrap_or_default()
        }
    });
    spawn_local(async move {
        if let Ok(c) = api::get::<BackupConfig>("/api/v1/system/backup").await {
            enabled.set(c.enabled);
            repo.set(c.repo);
            branch.set(c.branch);
            prefix.set(c.path_prefix);
            token_set.set(c.token_set);
        }
    });
    let save = move |_| {
        spawn_local(async move {
            let req = BackupConfig {
                enabled: enabled.get_untracked(),
                repo: repo.get_untracked(),
                branch: branch.get_untracked(),
                path_prefix: prefix.get_untracked(),
                token: Some(token.get_untracked()).filter(|t| !t.is_empty()),
                token_set: false,
            };
            match api::call::<BackupConfig>("PUT", "/api/v1/system/backup", js_json(&req)).await {
                Ok(c) => {
                    token.set(String::new());
                    token_set.set(c.token_set);
                    ctx.ok("Backup settings saved");
                }
                Err(e) => ctx.err(&e),
            }
        });
    };
    let run = move |_| {
        spawn_local(async move {
            match api::call::<serde_json::Value>(
                "POST",
                "/api/v1/system/backup/run",
                serde_json::json!({}),
            )
            .await
            {
                Ok(v) => {
                    ctx.ok(v["message"].as_str().unwrap_or("Backup done"));
                    ctx.refresh();
                }
                Err(e) => ctx.err(&e),
            }
        });
    };
    view! {
        <div class="panel"><h4>"Backup to GitHub"</h4>
            <p class="muted">"The database is encrypted with the server's master key and pushed to a PRIVATE repository daily (last 14 kept). The master key is never uploaded: keep your own copy (shown once at install; run "<code>"rustle key export"</code>" on the server)."</p>
            <div class="form">
                <label class="check"><input type="checkbox" prop:checked=move || enabled.get() on:change=move |e| enabled.set(event_target_checked(&e))/>" Enabled"</label>
                <label>"Repository (owner/name, must be private)"</label><input prop:value=move || repo.get() on:input=move |e| repo.set(event_target_value(&e))/>
                <label>"Branch"</label><input prop:value=move || branch.get() on:input=move |e| branch.set(event_target_value(&e))/>
                <label>"Folder"</label><input prop:value=move || prefix.get() on:input=move |e| prefix.set(event_target_value(&e))/>
                <label>{move || if token_set.get() { "Token (leave empty to keep the saved one)" } else { "Token (fine-grained, Contents: read & write on that repo)" }}</label>
                <input type="password" prop:value=move || token.get() on:input=move |e| token.set(event_target_value(&e))/>
                <div class="actions"><button class="primary" on:click=save>"Save"</button><button on:click=run>"Back up now"</button></div>
            </div>
            <Suspense fallback=|| ()>{move || runs.get().map(|v| view! { <table><tbody>{v.into_iter().take(5).map(|r| view! { <tr><td>{fmt_ts(r.ts)}</td><td>{r.status}</td><td class="mono small">{r.remote_path}</td></tr> }).collect_view()}</tbody></table> })}</Suspense>
        </div>
    }
}

#[component]
fn Updates() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let info = LocalResource::new(move || {
        ctx.tick.get();
        async move { api::get::<UpdateInfo>("/api/v1/system/update").await }
    });
    view! {
        <div class="panel"><h4>"Updates"</h4>
            <Suspense fallback=|| view! { <p class="muted">"Checking…"</p> }>{move || info.get().map(|r| match r {
                Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                Ok(i) => view! {
                    <p>{format!("Installed: {}", i.current)}{i.latest.clone().map(|l| format!("  ·  Latest: {l}"))}</p>
                    {i.reason.clone().map(|r| view! { <p class="muted">{r}</p> })}
                    {(i.enabled && i.update_available).then(|| view! { <button class="primary" on:click=move |_| {
                        if !confirm("Install the update now? Rustle restarts; running sites keep serving.") { return; }
                        spawn_local(async move {
                            match api::call::<serde_json::Value>("POST", "/api/v1/system/update", serde_json::json!({})).await {
                                Ok(_) => ctx.ok("Installed. Rustle is restarting; reload in a few seconds."),
                                Err(e) => ctx.err(&e),
                            }
                        })
                    }>"Update now"</button> })}
                    <p class="muted small">"Updates are accepted only when signed with the project's release key; a failed start rolls back automatically."</p>
                }.into_any(),
            })}</Suspense>
        </div>
    }
}

#[component]
fn Maintenance() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <div class="panel"><h4>"Maintenance"</h4>
            <p class="muted">"Removes Docker images that no container uses and that are not needed for rollback. Volumes are never touched."</p>
            <button on:click=move |_| {
                spawn_local(async move {
                    match api::call::<PruneResp>("POST", "/api/v1/system/prune", serde_json::json!({})).await {
                        Ok(r) => ctx.ok(&format!("Removed {} images, freed {:.1} MB", r.images_deleted, r.reclaimed_bytes as f64 / 1_048_576.0)),
                        Err(e) => ctx.err(&e),
                    }
                })
            }>"Prune unused images"</button>
        </div>
    }
}
