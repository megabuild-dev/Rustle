use crate::api::ApiErr;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

#[derive(Clone, Copy)]
pub struct Ctx {
    pub user: RwSignal<Option<String>>,
    pub toast: RwSignal<Option<(bool, String)>>,
    /// Bumped to make resources refetch.
    pub tick: RwSignal<u32>,
    pub wizard: RwSignal<bool>,
}

impl Ctx {
    pub fn new() -> Self {
        Ctx {
            user: RwSignal::new(None),
            toast: RwSignal::new(None),
            tick: RwSignal::new(0),
            wizard: RwSignal::new(false),
        }
    }
    pub fn refresh(&self) {
        self.tick.update(|t| *t += 1);
    }
    pub fn err(&self, e: &ApiErr) {
        if e.status == 401 {
            self.user.set(None);
        } else {
            self.say(false, &e.message);
        }
    }
    pub fn ok(&self, m: &str) {
        self.say(true, m);
    }
    pub fn say(&self, good: bool, m: &str) {
        self.toast.set(Some((good, m.to_string())));
        let t = self.toast;
        set_timeout(
            move || t.set(None),
            std::time::Duration::from_secs(if good { 3 } else { 7 }),
        );
    }
}

pub fn copy_text(s: &str) {
    let Some(w) = web_sys::window() else { return };
    let nav = w.navigator();
    if let Ok(cb) = js_sys::Reflect::get(&nav, &"clipboard".into()) {
        if let Ok(f) = js_sys::Reflect::get(&cb, &"writeText".into()) {
            if let Ok(f) = f.dyn_into::<js_sys::Function>() {
                let _ = f.call1(&cb, &s.into());
            }
        }
    }
}

pub fn confirm(msg: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.confirm_with_message(msg).ok())
        .unwrap_or(false)
}

pub fn fmt_ts(ts: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64 * 1000.0));
    d.to_iso_string()
        .as_string()
        .map(|s| s.replace('T', " ").chars().take(16).collect())
        .unwrap_or_default()
}

pub fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn digest_short(d: &Option<String>) -> String {
    match d {
        Some(x) => match x.split_once("sha256:") {
            Some((_, h)) => format!("sha256:{}", short(h, 10)),
            None => short(x, 20),
        },
        None => "-".into(),
    }
}

pub fn pct(used: f64, total: f64) -> f64 {
    if total <= 0.0 {
        0.0
    } else {
        (used / total * 100.0).clamp(0.0, 100.0)
    }
}

#[component]
pub fn Bar(used: f64, total: f64, #[prop(default = "ok")] tone: &'static str) -> impl IntoView {
    let p = pct(used, total);
    let tone = if p > 90.0 {
        "bad"
    } else if p > 75.0 {
        "warn"
    } else {
        tone
    };
    view! { <div class="bar"><div class=format!("fill {tone}") style=format!("width:{p:.0}%")></div></div> }
}

#[component]
pub fn Badge(#[prop(into)] status: String) -> impl IntoView {
    let tone = match status.as_str() {
        "running" | "success" | "ok" => "good",
        "failed" | "stopped" => "bad",
        "deploying" | "pending" | "running…" => "warn",
        _ => "muted",
    };
    view! { <span class=format!("badge {tone}")>{status}</span> }
}

#[component]
pub fn Toast() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <Show when=move || ctx.toast.get().is_some()>
            <div class=move || if ctx.toast.get().map(|t| t.0).unwrap_or(true) { "toast good" } else { "toast bad" }
                 on:click=move |_| ctx.toast.set(None)>
                {move || ctx.toast.get().map(|t| t.1).unwrap_or_default()}
            </div>
        </Show>
    }
}

/// A read-only code block with a copy button.
#[component]
pub fn Code(#[prop(into)] title: String, #[prop(into)] text: String) -> impl IntoView {
    let t = text.clone();
    let ctx = expect_context::<Ctx>();
    view! {
        <div class="code">
            <div class="code-head"><span>{title}</span>
                <button class="ghost sm" on:click=move |_| { copy_text(&t); ctx.ok("Copied"); }>"Copy"</button></div>
            <pre>{text}</pre>
        </div>
    }
}
