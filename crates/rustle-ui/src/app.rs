use crate::api;
use crate::common::{Ctx, Toast};
use crate::{cron, overview, settings, sites, wizard};
use leptos::prelude::*;
use leptos::task::spawn_local;
use rustle_types::{LoginReq, LoginResp};

#[derive(Clone, PartialEq, Debug)]
pub enum Route {
    Overview,
    Sites,
    Site(String, String),
    Cron,
    Settings,
}

pub fn parse_route(hash: &str) -> Route {
    let h = hash.trim_start_matches('#').trim_start_matches('/');
    let parts: Vec<&str> = h.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        ["sites"] => Route::Sites,
        ["sites", id] => Route::Site(id.to_string(), "overview".into()),
        ["sites", id, tab] => Route::Site(id.to_string(), tab.to_string()),
        ["cron"] => Route::Cron,
        ["settings"] => Route::Settings,
        _ => Route::Overview,
    }
}

fn current_route() -> Route {
    let hash = web_sys::window().and_then(|w| w.location().hash().ok()).unwrap_or_default();
    parse_route(&hash)
}

#[component]
pub fn App() -> impl IntoView {
    let ctx = Ctx::new();
    provide_context(ctx);
    let checked = RwSignal::new(false);
    spawn_local(async move {
        if let Ok(me) = api::get::<LoginResp>("/api/v1/auth/me").await {
            api::set_csrf(me.csrf);
            ctx.user.set(Some(me.username));
        }
        checked.set(true);
    });
    view! {
        <Toast/>
        <Show when=move || checked.get() fallback=|| view! { <div class="boot">"Loading…"</div> }>
            <Show when=move || ctx.user.get().is_some() fallback=|| view! { <Login/> }>
                <Shell/>
            </Show>
        </Show>
    }
}

#[component]
fn Login() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let user = RwSignal::new("admin".to_string());
    let pass = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        spawn_local(async move {
            let body = api::js_json(&LoginReq { username: user.get_untracked(), password: pass.get_untracked() });
            match api::call::<LoginResp>("POST", "/api/v1/auth/login", body).await {
                Ok(me) => {
                    api::set_csrf(me.csrf);
                    pass.set(String::new());
                    ctx.user.set(Some(me.username));
                }
                Err(e) => ctx.err(&e),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="login">
            <form on:submit=submit>
                <h1>"Rustle"</h1>
                <p class="muted">"Sign in to your server"</p>
                <label>"Username"</label>
                <input type="text" autocomplete="username" prop:value=move || user.get() on:input=move |e| user.set(event_target_value(&e))/>
                <label>"Password"</label>
                <input type="password" autocomplete="current-password" prop:value=move || pass.get() on:input=move |e| pass.set(event_target_value(&e))/>
                <button class="primary" type="submit" disabled=move || busy.get()>{move || if busy.get() { "Signing in…" } else { "Sign in" }}</button>
            </form>
        </div>
    }
}

#[component]
fn Shell() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let route = RwSignal::new(current_route());
    window_event_listener(leptos::ev::hashchange, move |_| route.set(current_route()));
    let logout = move |_| {
        spawn_local(async move {
            let _ = api::call_unit("POST", "/api/v1/auth/logout", None).await;
            api::set_csrf(String::new());
            ctx.user.set(None);
        });
    };
    let nav = move |label: &'static str, href: &'static str, active: bool| {
        view! { <a href=href class:active=move || active>{label}</a> }
    };
    let _ = nav;
    view! {
        <div class="app">
            <aside>
                <div class="brand">"Rustle"<span class="ver">"light PaaS"</span></div>
                <nav>
                    <a href="#/" class:active=move || route.get() == Route::Overview>"Overview"</a>
                    <a href="#/sites" class:active=move || matches!(route.get(), Route::Sites | Route::Site(..))>"Sites"</a>
                    <a href="#/cron" class:active=move || route.get() == Route::Cron>"Cron jobs"</a>
                    <a href="#/settings" class:active=move || route.get() == Route::Settings>"Settings"</a>
                </nav>
                <div class="grow"></div>
                <button class="ghost" on:click=logout>"Sign out"</button>
            </aside>
            <main>
                <header>
                    <h2>{move || match route.get() {
                        Route::Overview => "Overview".to_string(),
                        Route::Sites | Route::Site(..) => "Sites".to_string(),
                        Route::Cron => "Cron jobs".to_string(),
                        Route::Settings => "Settings".to_string(),
                    }}</h2>
                    <button class="primary" on:click=move |_| ctx.wizard.set(true)>"+ Add site"</button>
                </header>
                {move || match route.get() {
                    Route::Overview => view! { <overview::Overview/> }.into_any(),
                    Route::Sites => view! { <sites::SiteList/> }.into_any(),
                    Route::Site(id, tab) => view! { <sites::SiteDetail id=id tab=tab/> }.into_any(),
                    Route::Cron => view! { <cron::CronPage/> }.into_any(),
                    Route::Settings => view! { <settings::SettingsPage/> }.into_any(),
                }}
            </main>
            <Show when=move || ctx.wizard.get()><wizard::Wizard/></Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(parse_route(""), Route::Overview);
        assert_eq!(parse_route("#/"), Route::Overview);
        assert_eq!(parse_route("#/sites"), Route::Sites);
        assert_eq!(parse_route("#/sites/ab12"), Route::Site("ab12".into(), "overview".into()));
        assert_eq!(parse_route("#/sites/ab12/env"), Route::Site("ab12".into(), "env".into()));
        assert_eq!(parse_route("#/cron"), Route::Cron);
        assert_eq!(parse_route("#/settings"), Route::Settings);
        assert_eq!(parse_route("#/unknown/path"), Route::Overview);
    }
}
