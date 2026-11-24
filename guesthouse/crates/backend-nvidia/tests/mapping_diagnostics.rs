//! Unknown vendor mapping codes are visible without enabling depth tracing, and do not change RM calls.
use guesthouse_backend::{Backend, GpuRange, MapMemoryType, MapRequest, MemoryKind};
use guesthouse_backend_nvidia::fake_guest::start_on_card;
use guesthouse_backend_nvidia::rm::fake::{Call, FakeRm};
use guesthouse_backend_nvidia::rm::fake_card::{FAKE_ADA, FAKE_AMPERE};
use std::sync::Mutex;

struct Warnings(Mutex<Vec<String>>);
static WARNINGS: Warnings = Warnings(Mutex::new(Vec::new()));

impl log::Log for Warnings {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

#[test]
fn unknown_codes_warn_with_mapping_identity_without_overriding_the_rm_kind() {
    log::set_logger(&WARNINGS).unwrap();
    log::set_max_level(log::LevelFilter::Warn);
    for card in [&FAKE_AMPERE, &FAKE_ADA] {
        let mut backend = start_on_card(FakeRm::new(), card, card.device as u16).unwrap();
        let space = backend.create_address_space(&[GpuRange { start: 0x10000, end: 0x100000 }]).unwrap();
        let (memory, _) = backend.allocate_video_memory(0x20000, MemoryKind::PLAIN).unwrap();
        for (index, code) in [0, 1, 0x1000_0501, 0x1_1000_0501].into_iter().enumerate() {
            WARNINGS.0.lock().unwrap().clear();
            let before = backend.device().rm.calls.len();
            let address = 0x10000 + index as u64 * 0x10000;
            backend.map(&MapRequest {
                space, memory, offset: 0x1000, length: 0x2000, address,
                small_pages: true, gpu_cached: false,
                readable: true, writable: true, executable: false,
                memory_type: MapMemoryType::Default, driver_protection: code,
            }).unwrap();
            let translated = card.device == FAKE_AMPERE.device && code == 0x1000_0501;
            let warnings = WARNINGS.0.lock().unwrap();
            if code == 0 || translated {
                assert!(warnings.is_empty(), "{warnings:?}");
            } else {
                assert_eq!(warnings.len(), 1, "{warnings:?}");
                let warning = &warnings[0];
                let class = if card.device == FAKE_AMPERE.device { "c797" } else { "c997" };
                for field in [
                    "P1_MAP_KIND_UNKNOWN".to_owned(),
                    format!("driver_protection={code:#x}"),
                    format!("graphics_classes=[({class},"),
                    format!("client={:#x}", backend.device().client),
                    format!("memory={memory:?}"), format!("space={space:?}"),
                    format!("va={address:#x}"),
                    "backend=0x".to_owned(), "offset_bytes=0x1000".to_owned(),
                    "mapped_bytes=0x2000".to_owned(),
                ] {
                    assert!(warning.contains(&field), "missing {field}: {warning}");
                }
            }
            let calls = &backend.device().rm.calls[before..];
            assert!(calls.iter().any(|call| matches!(call, Call::MapDma(map) if map.address == address)));
            assert_eq!(calls.iter().filter(|call| matches!(call, Call::MapKind { .. })).count(),
                       usize::from(translated));
            assert!(!calls.iter().any(|call| matches!(call, Call::Alloc { .. })));
        }
    }
}
