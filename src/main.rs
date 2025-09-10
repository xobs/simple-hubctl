use std::time::Duration;
use usb_ids::FromId;

use nusb::{
    Device, DeviceInfo,
    transfer::{ControlIn, ControlOut, ControlType, Recipient, TransferError},
};

enum UsbDescriptorType {
    Hub = 0x29,
    SuperSpeedHub = 0x2a,
}

enum UsbDeviceClass {
    Hub = 0x09,
}

enum UsbRequest {
    GetStatus = 0,
    ClearFeature = 1,
    SetFeature = 3,
    GetDescriptor = 6,
}

/// Windows platforms must go through the Interface. Other platforms
/// may not even allow claiming the Interface.
struct HubControl(
    #[cfg(windows)] nusb::Interface,
    #[cfg(not(windows))] Device,
    bool, /* SuperSpeed */
);

impl HubControl {
    pub async fn new(device_info: &DeviceInfo) -> Result<Self, nusb::Error> {
        log::trace!(
            "Opening device {:04x}:{:04x}...",
            device_info.vendor_id(),
            device_info.product_id()
        );
        let is_superspeed = device_info.usb_version() >= 0x0300;
        let device = device_info.open().await?;

        Ok(HubControl(
            #[cfg(windows)]
            device.claim_interface(0).await?,
            #[cfg(not(windows))]
            device,
            is_superspeed,
        ))
    }

    pub async fn port_count(&self) -> Result<u8, TransferError> {
        let data = ControlIn {
            control_type: ControlType::Class,
            recipient: Recipient::Device,
            request: UsbRequest::GetDescriptor as _,
            value: (if self.1 {
                UsbDescriptorType::SuperSpeedHub
            } else {
                UsbDescriptorType::Hub
            } as u16)
                .to_be(),
            index: 0,
            length: 12,
        };
        let response = self.0.control_in(data, Duration::from_secs(5)).await?;
        log::trace!("Port count data: {response:02x?}");
        Ok(response[2])
    }

    pub async fn status(&self, port: u8) -> Result<bool, TransferError> {
        let data = ControlIn {
            control_type: ControlType::Class,
            recipient: Recipient::Other,
            request: UsbRequest::GetStatus as _,
            value: 0,
            index: port.into(),
            length: 4,
        };
        let response = self.0.control_in(data, Duration::from_secs(1)).await?;
        log::trace!("Port status data: {response:02x?}");
        Ok(response[1] & 1 != 0)
    }

    async fn set_port(&self, port: u8, enabled: bool) -> Result<(), TransferError> {
        let off = ControlOut {
            control_type: ControlType::Class,
            recipient: Recipient::Other,
            request: if enabled {
                UsbRequest::SetFeature
            } else {
                UsbRequest::ClearFeature
            } as _,
            value: 1 << 3, /* FEAT_POWER */
            index: port as _,
            data: &[],
        };
        log::trace!("Turning port {}...", if enabled { "on" } else { "off" });
        self.0.control_out(off, Duration::from_secs(5)).await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn off(&self, port: u8) -> Result<(), TransferError> {
        self.set_port(port, false).await
    }

    #[allow(dead_code)]
    pub async fn on(&self, port: u8) -> Result<(), TransferError> {
        self.set_port(port, true).await
    }

    pub async fn toggle(&self, port: u8) -> Result<(), TransferError> {
        self.set_port(port, !self.status(port).await?).await
    }
}

struct TogglablePort {
    name: String,
    enabled: bool,
    index: u8,
}

impl core::fmt::Display for TogglablePort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "    {}: {} -- {}",
            self.index,
            self.name,
            if self.enabled { "ON" } else { "off" }
        )
    }
}

struct TogglableDevice {
    name: String,
    control: HubControl,
    children: Vec<(String, bool /* port state */)>,
}

impl TogglableDevice {
    async fn new(device: SelectableDevice) -> Result<TogglableDevice, nusb::Error> {
        let control = HubControl::new(&device.info).await?;
        let mut children = vec![];
        for (index, child_name) in device.children.into_iter().enumerate() {
            let port_status = control.status(index as u8 + 1).await.ok().unwrap_or(false);
            children.push((child_name, port_status));
        }
        Ok(TogglableDevice {
            name: device.name,
            control,
            children,
        })
    }

    async fn toggle(&mut self, port: u8) -> Result<(), TransferError> {
        self.control.toggle(port).await?;
        self.children[port as usize - 1].1 = !self.children[port as usize - 1].1;
        Ok(())
    }

    fn selection(&self) -> Vec<TogglablePort> {
        let mut ret = vec![];
        for (index, child) in self.children.iter().enumerate() {
            ret.push(TogglablePort {
                name: child.0.clone(),
                enabled: child.1,
                index: index as u8 + 1,
            })
        }
        ret
    }
}

impl core::fmt::Display for TogglableDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

struct SelectableDevice {
    name: String,
    info: DeviceInfo,
    children: Vec<String>,
}

impl core::fmt::Display for SelectableDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.name)?;
        for (index, child) in self.children.iter().enumerate() {
            writeln!(f, "    {}: {child}", index + 1)?;
        }
        Ok(())
    }
}

fn get_name(device_info: &DeviceInfo) -> String {
    format!(
        "Hub {:04x}:{:04x} {} / {} / {} ({} / {}) @ {} {:?}",
        device_info.vendor_id(),
        device_info.product_id(),
        device_info.product_string().unwrap_or("[no product name]"),
        device_info
            .manufacturer_string()
            .unwrap_or("[no manufacturer]"),
        device_info.serial_number().unwrap_or("[no serial number]"),
        usb_ids::Vendor::from_id(device_info.vendor_id())
            .map(|v| v.name())
            .unwrap_or("[unknown vendor]"),
        usb_ids::Device::from_vid_pid(device_info.vendor_id(), device_info.product_id())
            .map(|v| v.name())
            .unwrap_or("[unknown product]"),
        device_info.bus_id(),
        device_info.port_chain()
    )
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    env_logger::init();
    let devices = nusb::list_devices().await?;
    let mut choices = vec![];
    let devices: Vec<DeviceInfo> = devices.collect();
    for device_info in &devices {
        let name = get_name(device_info);
        if device_info.class() != UsbDeviceClass::Hub as _ {
            continue;
        }
        let port_count = if let Ok(val) = HubControl::new(device_info).await {
            if let Ok(count) = val.port_count().await {
                Some(count)
            } else {
                None
            }
        } else {
            None
        };

        let mut children = vec![];
        if let Some(port_count) = port_count {
            children.resize_with(port_count as usize, || "<no device>".to_owned());
            let pc = device_info.port_chain();
            for child_device in &devices {
                if child_device.bus_id() != device_info.bus_id() {
                    continue;
                }
                let cpc = child_device.port_chain();
                if cpc.len() != pc.len() + 1 {
                    continue;
                }
                if cpc[0..pc.len()] != *pc {
                    continue;
                }
                let port_number = cpc[cpc.len() - 1];
                if port_number == 0 {
                    println!("ERROR: Port number is 0!");
                    continue;
                }
                let name = usb_ids::Device::from_vid_pid(
                    child_device.vendor_id(),
                    child_device.product_id(),
                )
                .map(|v| v.name().to_owned())
                .or_else(|| {
                    child_device.product_string().and_then(|ps| {
                        Some(format!(
                            "{ps} from {}",
                            usb_ids::Vendor::from_id(child_device.vendor_id())
                                .map(|v| v.name())
                                .unwrap_or("[unknown vendor]")
                        ))
                    })
                })
                .unwrap_or_else(|| "<unknown>".to_owned());
                children[port_number as usize - 1] = name;
            }
        } else {
            println!("Can't inquire port count from hub");
        }

        choices.push(SelectableDevice {
            name,
            info: device_info.clone(),
            children,
        });
    }

    let selection = inquire::Select::new("Select a hub", choices).prompt()?;
    let mut hub = TogglableDevice::new(selection).await?;

    let mut index = 0;
    while let Ok(port) = inquire::Select::new("Select a port to toggle", hub.selection())
        .with_starting_cursor(index)
        .prompt()
    {
        index = port.index as usize - 1;
        if let Err(e) = hub.toggle(port.index).await {
            println!("Couldn't toggle port {}: {e}", port.index);
        } else {
            println!(
                "Toggled port {} {}",
                port.index,
                if port.enabled { "off" } else { "ON" }
            );
        }
    }
    println!("Done");
    Ok(())
}
