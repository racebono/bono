//! Which GPU speech to text runs on, and whether flash attention is fast there.

use std::ffi::{CStr, c_char, c_int};
use whisper_rs::whisper_rs_sys::{
    ggml_backend_dev_count, ggml_backend_dev_get, ggml_backend_dev_get_props,
    ggml_backend_dev_props, ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU as GPU,
    ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU as INTEGRATED_GPU,
};

pub struct Gpu {
    /// Whisper's `gpu_device`. Its header doesn't say so, but it counts GPUs as [`devices`] does. If
    /// that changed, whisper would run on another GPU: slower, but not wrong.
    pub device: c_int,
    pub name: String,
    /// Whether flash attention is fast: much less work for the GPU, which the simulator is using too.
    pub flash: bool,
}

/// The GPU for speech to text, if there's one: the graphics card rather than the integrated GPU.
/// Loads the GPUs' drivers, so it's only called on workers.
pub fn choose() -> Option<Gpu> {
    let mut devices = devices();
    let device = best(&devices)?;
    let Device { name, pci, .. } = devices.swap_remove(device);
    #[cfg(feature = "vulkan")]
    let flash = pci.is_some_and(|pci| {
        fast_flash_attention(&pci).unwrap_or_else(|e| {
            log::warn!("Unable to ask Vulkan about {name}: {e}");
            false
        })
    });
    // It isn't known to be fast with other backends
    #[cfg(not(feature = "vulkan"))]
    let flash = {
        let _ = pci;
        false
    };
    Some(Gpu {
        device: c_int::try_from(device).ok()?,
        name,
        flash,
    })
}

struct Device {
    integrated: bool,
    name: String,
    /// Its PCI address, if it's known: "0000:01:00.0"
    pci: Option<String>,
}

/// The GPUs that ggml knows, in its order.
fn devices() -> Vec<Device> {
    // SAFETY: null, or a C string that's copied at once.
    let text = |text: *const c_char| {
        (!text.is_null()).then(|| {
            unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned()
        })
    };
    // SAFETY: there's a device for every index under the count, and all zeros is valid for the
    // properties (numbers, booleans and pointers), which are then filled in.
    (0..unsafe { ggml_backend_dev_count() })
        .map(|index| unsafe {
            let mut props: ggml_backend_dev_props = std::mem::zeroed();
            ggml_backend_dev_get_props(ggml_backend_dev_get(index), &mut props);
            props
        })
        .filter(|props| [GPU, INTEGRATED_GPU].contains(&props.type_))
        .map(|props| Device {
            integrated: props.type_ == INTEGRATED_GPU,
            name: text(props.description).unwrap_or_default(),
            pci: text(props.device_id),
        })
        .collect()
}

/// Which of `devices` to use.
fn best(devices: &[Device]) -> Option<usize> {
    (devices.iter().position(|device| !device.integrated))
        .or_else(|| (!devices.is_empty()).then_some(0))
}

/// Whether flash attention is fast on the GPU with this PCI address: ggml uses its matrix cores through
/// `VK_NV_cooperative_matrix2`. Without it, flash attention can be several times slower than none.
#[cfg(feature = "vulkan")]
fn fast_flash_attention(pci: &str) -> anyhow::Result<bool> {
    use ash::vk;

    // SAFETY: nothing from Vulkan is used once the instance is destroyed, and `entry` outlives it.
    unsafe {
        let entry = ash::Entry::load()?;
        // 1.1 for `get_physical_device_properties2`
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let info = vk::InstanceCreateInfo::default().application_info(&app);
        let instance = entry.create_instance(&info, None)?;
        let fast = instance.enumerate_physical_devices().and_then(|devices| {
            for device in devices {
                let version = instance.get_physical_device_properties(device).api_version;
                let extensions = instance.enumerate_device_extension_properties(device)?;
                let has =
                    |name| (extensions.iter()).any(|ext| ext.extension_name_as_c_str() == Ok(name));
                if version < vk::API_VERSION_1_1
                    || !has(ash::ext::pci_bus_info::NAME)
                    || !has(c"VK_NV_cooperative_matrix2")
                {
                    continue;
                }
                let mut bus = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
                let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut bus);
                instance.get_physical_device_properties2(device, &mut props);
                let address = pci_address(
                    bus.pci_domain,
                    bus.pci_bus,
                    bus.pci_device,
                    bus.pci_function,
                );
                if address == pci {
                    return Ok(true);
                }
            }
            Ok(false)
        });
        instance.destroy_instance(None);
        Ok(fast?)
    }
}

/// A PCI address as ggml writes it.
#[cfg(feature = "vulkan")]
fn pci_address(domain: u32, bus: u32, device: u32, function: u32) -> String {
    format!("{domain:04x}:{bus:02x}:{device:02x}.{function:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_graphics_card_comes_before_the_integrated_gpu() {
        let device = |integrated| Device {
            integrated,
            name: String::new(),
            pci: None,
        };
        assert_eq!(best(&[device(true), device(false), device(false)]), Some(1));
        assert_eq!(best(&[device(false), device(true)]), Some(0));
        assert_eq!(best(&[device(true)]), Some(0));
        assert_eq!(best(&[]), None);
    }

    /// As in ggml's header: "0000:01:00.0"
    #[cfg(feature = "vulkan")]
    #[test]
    fn pci_addresses_are_written_as_ggml_writes_them() {
        assert_eq!(pci_address(0, 1, 0, 0), "0000:01:00.0");
        assert_eq!(pci_address(0x1a, 0xc2, 0x1f, 7), "001a:c2:1f.7");
    }

    /// No card has this address. Must not crash either, with or without a Vulkan driver
    #[cfg(feature = "vulkan")]
    #[test]
    fn an_unknown_card_is_not_fast() {
        assert!(!fast_flash_attention("ffff:ff:1f.7").unwrap_or(false));
    }

    /// Must also not crash in builds with a GPU backend, whether or not there's a GPU
    #[test]
    fn without_a_gpu_backend_there_is_no_gpu() {
        let gpu = choose();
        assert!(super::super::whisper::GPU_BACKEND || gpu.is_none());
    }
}
