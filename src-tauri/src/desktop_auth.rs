use tauri::plugin::TauriPlugin;

pub fn plugin<R: tauri::Runtime>() -> TauriPlugin<R> {
    tauri::plugin::Builder::new("desktop-auth")
        .js_init_script(include_str!("desktop_auth.js"))
        .build()
}
