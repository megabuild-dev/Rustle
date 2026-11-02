use crate::api::{self, js_json};
use crate::common::{confirm, fmt_ts, Badge, Ctx};
use leptos::prelude::*;
use leptos::task::spawn_local;
use rustle_types::*;

#[component]
pub fn CronPage() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let jobs = LocalResource::new(move || {
        ctx.tick.get();
        async move { api::get::<Vec<CronJobDto>>("/api/v1/cron").await }
    });
    let sites = LocalResource::new(move || async move {
        api::get::<Vec<SiteDto>>("/api/v1/sites")
            .await
            .unwrap_or_default()
    });
    let open = RwSignal::new(None::<String>);
    let runs = RwSignal::new(Vec::<CronRunDto>::new());
    let (name, sched, site, argv) = (
        RwSignal::new(String::new()),
        RwSignal::new("0 3 * * *".to_string()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
    );

    let http_mode = RwSignal::new(false);
    let http_path = RwSignal::new(String::new());
    let add = move |_| {
        let args: Vec<String> = argv
            .get_untracked()
            .lines()
            .map(|l| l.to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let http = http_mode.get_untracked();
        let req = CreateCronReq {
            name: name.get_untracked(),
            schedule: sched.get_untracked(),
            site_id: site.get_untracked(),
            argv: if http { vec![] } else { args },
            http_path: http.then(|| http_path.get_untracked()),
        };
        spawn_local(async move {
            match api::call::<CronJobDto>("POST", "/api/v1/cron", js_json(&req)).await {
                Ok(_) => {
                    ctx.ok("Job created");
                    name.set(String::new());
                    argv.set(String::new());
                    ctx.refresh();
                }
                Err(e) => ctx.err(&e),
            }
        });
    };

    view! {
        <p class="muted">"System jobs run inside Rustle. Site jobs start a throw-away copy of the site's image with the exact arguments you give (no shell), then remove it. Times are UTC."</p>
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || jobs.get().map(|r| match r {
                Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
                Ok(v) => view! {
                    <table>
                        <thead><tr><th>"Job"</th><th>"Schedule (UTC)"</th><th>"Last run"</th><th>"On"</th><th></th></tr></thead>
                        <tbody>{v.into_iter().map(|j| {
                            let (id1, id2, id3, id4) = (j.id.clone(), j.id.clone(), j.id.clone(), j.id.clone());
                            let enabled = j.enabled;
                            let is_site = j.kind == "site";
                            let argv_txt = j.argv.clone().map(|a| a.join(" ")).map(|a| if j.kind == "http" { format!("GET {a}") } else { a }).unwrap_or_default();
                            view! {
                                <tr>
                                    <td><div>{j.name.clone()}</div>{(!argv_txt.is_empty()).then(|| view! { <div class="mono muted small">{argv_txt.clone()}</div> })}</td>
                                    <td class="mono">{j.schedule.clone()}</td>
                                    <td>{j.last_status.clone().map(|s| view! { <Badge status=s/> })} " " {j.last_run_at.map(fmt_ts).unwrap_or("never".into())}</td>
                                    <td><input type="checkbox" prop:checked=enabled on:change=move |e| {
                                        let (id, on) = (id1.clone(), event_target_checked(&e));
                                        spawn_local(async move {
                                            match api::call::<CronJobDto>("PATCH", &format!("/api/v1/cron/{id}"), serde_json::json!({"enabled": on})).await {
                                                Ok(_) => ctx.refresh(), Err(e) => ctx.err(&e) }
                                        })
                                    }/></td>
                                    <td class="actions">
                                        <button class="sm" on:click=move |_| {
                                            let id = id2.clone();
                                            spawn_local(async move {
                                                match api::call_unit("POST", &format!("/api/v1/cron/{id}/run"), None).await {
                                                    Ok(()) => { ctx.ok("Started"); ctx.refresh(); } Err(e) => ctx.err(&e) }
                                            })
                                        }>"Run now"</button>
                                        <button class="sm" on:click=move |_| {
                                            let id = id3.clone();
                                            if open.get_untracked().as_deref() == Some(id.as_str()) { open.set(None); return; }
                                            open.set(Some(id.clone()));
                                            spawn_local(async move {
                                                match api::get::<Vec<CronRunDto>>(&format!("/api/v1/cron/{id}/runs")).await {
                                                    Ok(r) => runs.set(r), Err(e) => ctx.err(&e) }
                                            })
                                        }>"History"</button>
                                        {is_site.then(|| view! { <button class="danger sm" on:click=move |_| {
                                            if !confirm("Delete this job?") { return; }
                                            let id = id4.clone();
                                            spawn_local(async move {
                                                match api::call_unit("DELETE", &format!("/api/v1/cron/{id}"), None).await {
                                                    Ok(()) => ctx.refresh(), Err(e) => ctx.err(&e) }
                                            })
                                        }>"Delete"</button> })}
                                    </td>
                                </tr>
                                <Show when={let id = j.id.clone(); move || open.get().as_deref() == Some(id.as_str())}>
                                    <tr><td colspan="5">
                                        {move || runs.get().into_iter().take(5).map(|r| view! {
                                            <div class="run">
                                                <b>{fmt_ts(r.started_at)}</b>" "
                                                <Badge status=match (r.finished_at, r.exit_code) { (None, _) => "running", (_, Some(0)) => "success", _ => "failed" }/>
                                                <pre class="logs small">{r.log_tail}</pre>
                                            </div>
                                        }).collect_view()}
                                    </td></tr>
                                </Show>
                            }
                        }).collect_view()}</tbody>
                    </table>
                }.into_any(),
            })}
        </Suspense>
        <div class="panel">
            <h4>"New site job"</h4>
            <div class="form">
                <label>"Name"</label><input prop:value=move || name.get() on:input=move |e| name.set(event_target_value(&e))/>
                <label>"Site"</label>
                <select on:change=move |e| site.set(event_target_value(&e))>
                    <option value="">"Select…"</option>
                    {move || sites.get().unwrap_or_default().into_iter().map(|s| view! { <option value=s.id.clone()>{s.name.clone()}</option> }).collect_view()}
                </select>
                <label>"Schedule (cron, UTC)"</label><input class="mono" prop:value=move || sched.get() on:input=move |e| sched.set(event_target_value(&e))/>
                <label class="check"><input type="checkbox" prop:checked=move || http_mode.get() on:change=move |e| http_mode.set(event_target_checked(&e))/>" Call a path of the running site instead of running a command (sends Authorization: Bearer $CRON_SECRET when the site defines CRON_SECRET)"</label>
                <Show when=move || http_mode.get()>
                    <label>"Path"</label>
                    <input class="mono" placeholder="/api/cron/cleanup" prop:value=move || http_path.get() on:input=move |e| http_path.set(event_target_value(&e))/>
                </Show>
                <Show when=move || !http_mode.get()>
                <label>"Command, one argument per line"</label>
                <textarea class="mono" rows="3" placeholder="node\nscripts/cleanup.js" prop:value=move || argv.get() on:input=move |e| argv.set(event_target_value(&e))></textarea>
                </Show>
                <button class="primary" on:click=add>"Create job"</button>
            </div>
        </div>
    }
}
