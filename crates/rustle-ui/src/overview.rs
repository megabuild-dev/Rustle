use crate::api;
use crate::common::{Badge, Bar, Ctx};
use leptos::prelude::*;
use leptos::task::spawn_local;
use rustle_types::{SiteDto, SystemStats};
use std::time::Duration;

#[component]
pub fn Overview() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let stats = RwSignal::new(None::<SystemStats>);
    let sites = LocalResource::new(move || {
        ctx.tick.get();
        async move { api::get::<Vec<SiteDto>>("/api/v1/sites").await }
    });
    // Sampling happens on the server only while a request arrives, so polling here is the cost model.
    let poll = move || {
        spawn_local(async move {
            match api::get::<SystemStats>("/api/v1/system/stats").await {
                Ok(s) => stats.set(Some(s)),
                Err(e) => ctx.err(&e),
            }
        })
    };
    poll();
    let handle = set_interval_with_handle(poll, Duration::from_secs(3)).ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });

    view! {
        <Show when=move || stats.get().is_some_and(|s| s.gateway_external && !s.admin_domain_set)>
            <div class="banner warn">"Rustle is sharing your existing Caddy. Set a dashboard domain in Settings to open this dashboard through it; until then use the SSH tunnel."</div>
        </Show>
        <Show when=move || stats.get().is_some_and(|s| s.public_ip_mode)>
            <div class="banner warn">"The dashboard uses a self-signed certificate on the server IP. Set a domain in Settings to get a trusted HTTPS address."</div>
        </Show>
        <Show when=move || stats.get().is_some_and(|s| s.degraded_reason.is_some())>
            <div class="banner bad">{move || stats.get().and_then(|s| s.degraded_reason).unwrap_or_default()}</div>
        </Show>
        <div class="cards">
            {move || stats.get().map(|s| {
                let ram_total = s.mem_total_mb as f64;
                view! {
                    <div class="card"><div class="k">"CPU"</div><div class="v">{format!("{:.0}%", s.cpu_percent)}</div><Bar used=s.cpu_percent as f64 total=100.0/></div>
                    <div class="card"><div class="k">"RAM"</div><div class="v">{format!("{} / {} MB", s.mem_used_mb, s.mem_total_mb)}</div><Bar used=s.mem_used_mb as f64 total=ram_total/></div>
                    <div class="card"><div class="k">"Swap"</div><div class="v">{format!("{} / {} MB", s.swap_used_mb, s.swap_total_mb)}</div><Bar used=s.swap_used_mb as f64 total=s.swap_total_mb as f64 tone="purple"/></div>
                    <div class="card"><div class="k">"Disk"</div><div class="v">{format!("{:.1} / {:.1} GB", s.disk_used_gb, s.disk_total_gb)}</div><Bar used=s.disk_used_gb as f64 total=s.disk_total_gb as f64/></div>
                }
            })}
        </div>
        {move || stats.get().map(|s| view! {
            <div class="strip">
                <span>"Gateway: "<Badge status=if s.gateway_ok { "ok" } else { "failed" }/></span>
                <span>"Docker: "<Badge status=if s.runtime_ok { "ok" } else { "failed" }/></span>
                <span>"Uptime: "{format!("{}d {}h", s.uptime_secs / 86400, (s.uptime_secs % 86400) / 3600)}</span>
                <span>"Rustle process: "<b>{format!("{:.1} MB", s.rustle_rss_kb as f64 / 1024.0)}</b></span>
                <span>"Allocated to sites: "<b>{format!("{} / {} MB", s.allocated_mb, s.budget_mb)}</b></span>
            </div>
        })}
        <h3>"Sites"</h3>
        <Suspense fallback=|| view! { <p class="muted">"Loading…"</p> }>
            {move || sites.get().map(|r| match r {
                Ok(list) if list.is_empty() => view! { <p class="muted">"No sites yet. Click “Add site” to deploy your first app."</p> }.into_any(),
                Ok(list) => view! { <div class="list">{list.into_iter().map(|s| view! { <SiteRow s=s/> }).collect_view()}</div> }.into_any(),
                Err(e) => view! { <p class="err">{e.to_string()}</p> }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
pub fn SiteRow(s: SiteDto) -> impl IntoView {
    let href = format!("#/sites/{}", s.id);
    let used = s.ram_used_mb.unwrap_or(0) as f64;
    view! {
        <a class="row" href=href>
            <div>
                <div class="name">{s.name.clone()} " " <Badge status=s.status.clone()/></div>
                <div class="muted mono">{format!("https://{}", s.domain)}</div>
            </div>
            <div class="ram">
                <div class="muted">{format!("RAM {} / {} MB", s.ram_used_mb.map(|v| v.to_string()).unwrap_or("-".into()), s.ram_limit_mb)}</div>
                <Bar used=used total=s.ram_limit_mb as f64/>
            </div>
        </a>
    }
}
