#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Wayland: WebKitGTK's DMA-BUF renderer asks for buffers some drivers
    // (NVIDIA's above all) cannot give, and the compositor drops chatir:
    // "Error 71 (Protocol error)". NVIDIA's driver, X11 too: WebKit's GPU
    // path probes it through Mesa, fails, and its page has grown past
    // WebKit's 16 GB limit on a fresh start (a Blackwell card): drawn on
    // the CPU there. Each off unless set; a chat needs no more.
    #[cfg(target_os = "linux")]
    {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let nvidia = std::path::Path::new("/proc/driver/nvidia").exists();
        let off = [
            ("WEBKIT_DISABLE_DMABUF_RENDERER", wayland),
            ("__NV_DISABLE_EXPLICIT_SYNC", wayland && nvidia),
            ("WEBKIT_DISABLE_COMPOSITING_MODE", nvidia),
        ];
        for (k, on) in off {
            if on && std::env::var_os(k).is_none() {
                // Before any thread starts.
                unsafe { std::env::set_var(k, "1") };
            }
        }
    }
    chatir_app::run()
}
