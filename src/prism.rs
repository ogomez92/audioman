use std::ffi::{c_char, CString};

#[repr(C)]
pub struct PrismContext {
    _unused: [u8; 0],
}

#[repr(C)]
pub struct PrismBackend {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PrismConfig {
    pub version: u8,
}

#[link(name = "prism")]
extern "C" {
    fn prism_config_init() -> PrismConfig;
    fn prism_init(cfg: *mut PrismConfig) -> *mut PrismContext;
    fn prism_shutdown(ctx: *mut PrismContext);
    fn prism_registry_acquire_best(ctx: *mut PrismContext) -> *mut PrismBackend;
    fn prism_backend_initialize(backend: *mut PrismBackend) -> i32;
    fn prism_backend_speak(
        backend: *mut PrismBackend,
        text: *const c_char,
        interrupt: bool,
    ) -> i32;
}

pub struct Speaker {
    ctx: *mut PrismContext,
    backend: *mut PrismBackend,
}

impl Speaker {
    pub fn new() -> Option<Self> {
        unsafe {
            let mut cfg = prism_config_init();
            let ctx = prism_init(&mut cfg);
            if ctx.is_null() {
                return None;
            }
            let backend = prism_registry_acquire_best(ctx);
            if backend.is_null() {
                prism_shutdown(ctx);
                return None;
            }
            let _ = prism_backend_initialize(backend);
            Some(Self { ctx, backend })
        }
    }

    pub fn speak(&self, text: &str) {
        let Ok(c) = CString::new(text.replace('\0', " ")) else {
            return;
        };
        unsafe {
            let _ = prism_backend_speak(self.backend, c.as_ptr(), true);
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        unsafe {
            prism_shutdown(self.ctx);
        }
    }
}
