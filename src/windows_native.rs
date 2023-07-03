//! The implementation which uses the C library to perform operations

use std::{
    ffi::CStr,
    fmt::{self, Debug},
};
use std::ffi::{c_void, CString};
use std::mem::{size_of, zeroed};
use std::ptr::{addr_of_mut, null, null_mut};

use libc::{c_int, size_t, wchar_t};
use windows_sys::core::{GUID, PCWSTR};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_PropertyW, CM_Get_DevNode_PropertyW, CM_Get_Parent, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CR_BUFFER_SMALL, CR_SUCCESS};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{HIDD_ATTRIBUTES, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetManufacturerString, HidD_GetPreparsedData, HidD_GetProductString, HidD_GetSerialNumberString, HidP_GetCaps};
use windows_sys::Win32::Devices::Properties::{DEVPKEY_Device_CompatibleIds, DEVPKEY_Device_HardwareIds, DEVPKEY_Device_InstanceId, DEVPKEY_Device_Manufacturer, DEVPKEY_NAME, DEVPROP_TYPE_STRING, DEVPROP_TYPE_STRING_LIST, DEVPROPKEY, DEVPROPTYPE};
use windows_sys::Win32::Foundation::{BOOLEAN, CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::EnhancedStorage::{PKEY_DeviceInterface_Bluetooth_DeviceAddress, PKEY_DeviceInterface_Bluetooth_Manufacturer, PKEY_DeviceInterface_Bluetooth_ModelNumber};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

use crate::{ffi, DeviceInfo, HidDeviceBackendBase, HidError, HidResult, WcharString, HidDeviceBackendWindows, BusType};

const STRING_BUF_LEN: usize = 128;

macro_rules! ensure {
    ($cond:expr, $result:expr) => {
        if !($cond) {
            return $result;
        }
    };
}


fn get_interface_list() -> Vec<u16> {
    let interface_class_guid = unsafe {
        let mut guid = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        guid
    };

    let mut device_interface_list = Vec::new();
    loop {
        let mut len = 0;
        let cr = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &interface_class_guid,
                null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
        };
        assert_eq!(cr, CR_SUCCESS, "Failed to get size of HID device interface list");
        device_interface_list.resize(len as usize, 0);
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(
                &interface_class_guid,
                null(),
                device_interface_list.as_mut_ptr(),
                device_interface_list.len() as u32,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT
            )
        };
        assert!(cr == CR_SUCCESS || cr == CR_BUFFER_SMALL, "Failed to get HID device interface list");
        if cr == CR_SUCCESS {
            break;
        }
    }
    device_interface_list
}

fn open_device(path: PCWSTR, open_rw: bool) -> Option<HANDLE> {
    let handle = unsafe {
        CreateFileW(
            path,
            match open_rw {
                true => GENERIC_WRITE | GENERIC_READ,
                false => 0
            },
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            0
        )
    };
    ensure!(handle != INVALID_HANDLE_VALUE, None);
    Some(handle)
}

fn read_string(func: unsafe extern "system" fn (HANDLE, *mut c_void, u32) -> BOOLEAN, handle: HANDLE) -> WcharString {
    //Return empty string on failure to match the c implementation
    let mut string = [0u16; 256];
    if unsafe { func(handle, string.as_mut_ptr() as _, (size_of::<u16>() * string.len()) as u32) } != 0 {
        string
            .split(|c| *c == 0)
            .map(u16str_to_wstring)
            .next()
            .unwrap_or_else(|| WcharString::String(String::new()))
    } else {
        //WcharString::None
        WcharString::String(String::new())
    }
}

fn get_device_info(path: &[u16], handle: HANDLE) -> DeviceInfo {
    let attrib = unsafe {
        let mut attrib = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            ..zeroed()
        };
        HidD_GetAttributes(handle, &mut attrib);
        attrib
    };
    let caps = unsafe {
        let mut caps = zeroed();
        let mut pp_data = 0;
        if HidD_GetPreparsedData(handle, &mut pp_data) != 0 {
            HidP_GetCaps(pp_data, &mut caps);
            HidD_FreePreparsedData(pp_data);
        }
        caps
    };


    let mut dev = DeviceInfo {
        path: CString::new(String::from_utf16(path).unwrap()).unwrap(),
        vendor_id: attrib.VendorID,
        product_id: attrib.ProductID,
        serial_number: read_string(HidD_GetSerialNumberString, handle),
        release_number: attrib.VersionNumber,
        manufacturer_string: read_string(HidD_GetManufacturerString, handle),
        product_string: read_string(HidD_GetProductString, handle),
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        interface_number: -1,
        bus_type: BusType::Unknown,
    };

    get_internal_info(path.as_ptr(), &mut dev);
    dev
}

fn get_internal_info(interface_path: PCWSTR, dev: &mut DeviceInfo) -> Option<()> {
    let device_id = get_device_interface_property(interface_path, &DEVPKEY_Device_InstanceId, DEVPROP_TYPE_STRING)?;

    let dev_node = unsafe {
        let mut node = 0;
        let cr = CM_Locate_DevNodeW(&mut node, device_id.as_ptr() as _, CM_LOCATE_DEVNODE_NORMAL);
        ensure!(cr == CR_SUCCESS, None);
        get_dev_node_parent(node)?
    };

    let compatible_ids = get_devnode_property(dev_node, &DEVPKEY_Device_CompatibleIds, DEVPROP_TYPE_STRING_LIST)?;

    let bus_type = bytemuck::cast_slice(&compatible_ids)
        .split(|c| *c == 0)
        .filter_map(|compatible_id| match compatible_id {
            /* USB devices
		   https://docs.microsoft.com/windows-hardware/drivers/hid/plug-and-play-support
		   https://docs.microsoft.com/windows-hardware/drivers/install/standard-usb-identifiers */
            id if starts_with_ignore_case(id, "USB") => Some(InternalBuyType::Usb),
            /* Bluetooth devices
		   https://docs.microsoft.com/windows-hardware/drivers/bluetooth/installing-a-bluetooth-device */
            id if starts_with_ignore_case(id, "BTHENUM") => Some(InternalBuyType::Bluetooth),
            id if starts_with_ignore_case(id, "BTHLEDEVICE") => Some(InternalBuyType::BluetoothLE),
            /* I2C devices
		   https://docs.microsoft.com/windows-hardware/drivers/hid/plug-and-play-support-and-power-management */
            id if starts_with_ignore_case(id, "PNP0C50") => Some(InternalBuyType::I2c),
            /* SPI devices
		   https://docs.microsoft.com/windows-hardware/drivers/hid/plug-and-play-for-spi */
            id if starts_with_ignore_case(id, "PNP0C51") => Some(InternalBuyType::Spi),
            _ => None
        })
        .next()
        .unwrap_or(InternalBuyType::Unknown);
    dev.bus_type = bus_type.into();
    match bus_type {
        InternalBuyType::Usb => get_usb_info(dev, dev_node),
        InternalBuyType::BluetoothLE => get_ble_info(dev, dev_node),
        _ => None
    };

    Some(())
}

fn get_usb_info(dev: &mut DeviceInfo, mut dev_node: u32) -> Option<()> {
    let mut device_id = get_devnode_property(dev_node, &DEVPKEY_Device_InstanceId, DEVPROP_TYPE_STRING)?;

    to_upper(bytemuck::cast_slice_mut(&mut device_id));
    /* Check for Xbox Common Controller class (XUSB) device.
	   https://docs.microsoft.com/windows/win32/xinput/directinput-and-xusb-devices
	   https://docs.microsoft.com/windows/win32/xinput/xinput-and-directinput
	*/
    if extract_int_token_value(bytemuck::cast_slice(&device_id), "IG_").is_some() {
        dev_node = get_dev_node_parent(dev_node)?;
    }

    let mut hardware_ids = get_devnode_property(dev_node, &DEVPKEY_Device_HardwareIds, DEVPROP_TYPE_STRING_LIST)?;

    /* Get additional information from USB device's Hardware ID
	   https://docs.microsoft.com/windows-hardware/drivers/install/standard-usb-identifiers
	   https://docs.microsoft.com/windows-hardware/drivers/usbcon/enumeration-of-interfaces-not-grouped-in-collections
	*/
    for hardware_id in bytemuck::cast_slice_mut(&mut hardware_ids).split_mut(|c| *c == 0) {
        to_upper(hardware_id);
        if dev.release_number == 0 {
            if let Some(release_number) = extract_int_token_value(hardware_id, "REV_") {
                dev.release_number = release_number as u16;
            }
        }
        if dev.interface_number == -1 {
            if let Some(interface_number) = extract_int_token_value(hardware_id, "MI_") {
                dev.interface_number = interface_number as i32;
            }
        }
    }

    /* Try to get USB device manufacturer string if not provided by HidD_GetManufacturerString. */
    if dev.manufacturer_string().map_or(true, str::is_empty) {
        if let Some(manufacturer_string) = get_devnode_property(dev_node, &DEVPKEY_Device_Manufacturer, DEVPROP_TYPE_STRING) {
            dev.manufacturer_string = u16str_to_wstring(bytemuck::cast_slice(&manufacturer_string));
        }
    }

    /* Try to get USB device serial number if not provided by HidD_GetSerialNumberString. */
    if dev.serial_number().map_or(true, str::is_empty) {
        let mut usb_dev_node = dev_node;
        if dev.interface_number != -1 {
            /* Get devnode parent to reach out composite parent USB device.
               https://docs.microsoft.com/windows-hardware/drivers/usbcon/enumeration-of-the-composite-parent-device
            */
            usb_dev_node = get_dev_node_parent(dev_node)?;
        }

        let device_id = get_devnode_property(usb_dev_node, &DEVPKEY_Device_InstanceId, DEVPROP_TYPE_STRING)?;
        let device_id = bytemuck::cast_slice::<u8, u16>(&device_id);

        /* Extract substring after last '\\' of Instance ID.
		   For USB devices it may contain device's serial number.
		   https://docs.microsoft.com/windows-hardware/drivers/install/instance-ids
		*/
        if let Some(start) = device_id
            .rsplit(|c| *c != b'&' as u16)
            .next()
            .and_then(|s| s.iter().rposition(|c| *c != b'\\' as u16)) {
            dev.serial_number = u16str_to_wstring(&device_id[(start + 1)..]);
        }

    }

    if dev.interface_number == -1 {
        dev.interface_number = 0;
    }

    Some(())
}

/* HidD_GetProductString/HidD_GetManufacturerString/HidD_GetSerialNumberString is not working for BLE HID devices
   Request this info via dev node properties instead.
   https://docs.microsoft.com/answers/questions/401236/hidd-getproductstring-with-ble-hid-device.html
*/
fn get_ble_info(dev: &mut DeviceInfo, dev_node: u32) -> Option<()>{
    if dev.manufacturer_string().map_or(true, str::is_empty) {
        if let Some(manufacturer_string) = get_devnode_property(
            dev_node,
            (&PKEY_DeviceInterface_Bluetooth_Manufacturer as *const PROPERTYKEY) as _,
            DEVPROP_TYPE_STRING) {
            dev.manufacturer_string = u16str_to_wstring(bytemuck::cast_slice(&manufacturer_string));
        }
    }

    if dev.serial_number().map_or(true, str::is_empty) {
        if let Some(serial_number) = get_devnode_property(
            dev_node,
            (&PKEY_DeviceInterface_Bluetooth_DeviceAddress as *const PROPERTYKEY) as _,
            DEVPROP_TYPE_STRING) {
            dev.serial_number = u16str_to_wstring(bytemuck::cast_slice(&serial_number));
        }
    }

    if dev.product_string().map_or(true, str::is_empty) {
        let product_string = get_devnode_property(
            dev_node,
            (&PKEY_DeviceInterface_Bluetooth_ModelNumber as *const PROPERTYKEY) as _,
            DEVPROP_TYPE_STRING
        ).or_else(|| {
            /* Fallback: Get devnode grandparent to reach out Bluetooth LE device node */
            get_dev_node_parent(dev_node)
                .and_then(|parent_dev_node| get_devnode_property(parent_dev_node, &DEVPKEY_NAME, DEVPROP_TYPE_STRING))
        });
        if let Some(product_string) = product_string {
            dev.product_string = u16str_to_wstring(bytemuck::cast_slice(&product_string));
        }
    }

    Some(())
}

pub struct HidApiBackend;
impl HidApiBackend {
    pub fn get_hid_device_info_vector() -> HidResult<Vec<DeviceInfo>> {
        let mut device_vector = Vec::with_capacity(8);

        for device_interface in get_interface_list().split(|c| *c == 0) {
            //println!("{}", String::from_utf16_lossy(device_interface));

            if let Some(device_handle) = open_device(device_interface.as_ptr(), false) {
                device_vector.push(get_device_info(device_interface, device_handle));

                unsafe { CloseHandle(device_handle); }
            }

        }

        Ok(device_vector)
    }

    pub fn open(vid: u16, pid: u16) -> HidResult<HidDevice> {
        let device = unsafe { ffi::hid_open(vid, pid, std::ptr::null()) };

        if device.is_null() {
            match Self::check_error() {
                Ok(err) => Err(err),
                Err(e) => Err(e),
            }
        } else {
            Ok(HidDevice::from_raw(device))
        }
    }

    pub fn open_serial(vid: u16, pid: u16, sn: &str) -> HidResult<HidDevice> {
        let mut chars = sn.chars().map(|c| c as wchar_t).collect::<Vec<_>>();
        chars.push(0 as wchar_t);
        let device = unsafe { ffi::hid_open(vid, pid, chars.as_ptr()) };
        if device.is_null() {
            match Self::check_error() {
                Ok(err) => Err(err),
                Err(e) => Err(e),
            }
        } else {
            Ok(HidDevice::from_raw(device))
        }
    }

    pub fn open_path(device_path: &CStr) -> HidResult<HidDevice> {
        let device = unsafe { ffi::hid_open_path(device_path.as_ptr()) };

        if device.is_null() {
            match Self::check_error() {
                Ok(err) => Err(err),
                Err(e) => Err(e),
            }
        } else {
            Ok(HidDevice::from_raw(device))
        }
    }

    pub fn check_error() -> HidResult<HidError> {
        Ok(HidError::HidApiError {
            message: unsafe {
                match wchar_to_string(ffi::hid_error(std::ptr::null_mut())) {
                    WcharString::String(s) => s,
                    _ => return Err(HidError::HidApiErrorEmpty),
                }
            },
        })
    }
}

/// Converts a pointer to a `*const wchar_t` to a WcharString.
unsafe fn wchar_to_string(wstr: *const wchar_t) -> WcharString {
    if wstr.is_null() {
        return WcharString::None;
    }

    let mut char_vector: Vec<char> = Vec::with_capacity(8);
    let mut raw_vector: Vec<wchar_t> = Vec::with_capacity(8);
    let mut index: isize = 0;
    let mut invalid_char = false;

    let o = |i| *wstr.offset(i);

    while o(index) != 0 {
        use std::char;

        raw_vector.push(*wstr.offset(index));

        if !invalid_char {
            if let Some(c) = char::from_u32(o(index) as u32) {
                char_vector.push(c);
            } else {
                invalid_char = true;
            }
        }

        index += 1;
    }

    if !invalid_char {
        WcharString::String(char_vector.into_iter().collect())
    } else {
        WcharString::Raw(raw_vector)
    }
}

/// Convert the CFFI `HidDeviceInfo` struct to a native `HidDeviceInfo` struct
pub unsafe fn conv_hid_device_info(src: *mut ffi::HidDeviceInfo) -> HidResult<DeviceInfo> {
    Ok(DeviceInfo {
        path: CStr::from_ptr((*src).path).to_owned(),
        vendor_id: (*src).vendor_id,
        product_id: (*src).product_id,
        serial_number: wchar_to_string((*src).serial_number),
        release_number: (*src).release_number,
        manufacturer_string: wchar_to_string((*src).manufacturer_string),
        product_string: wchar_to_string((*src).product_string),
        usage_page: (*src).usage_page,
        usage: (*src).usage,
        interface_number: (*src).interface_number,
        bus_type: (*src).bus_type,
    })
}

/// Object for accessing HID device
pub struct HidDevice {
    _hid_device: *mut ffi::HidDevice,
}

impl HidDevice {
    pub fn from_raw(device: *mut ffi::HidDevice) -> Self {
        Self {
            _hid_device: device,
        }
    }
}

unsafe impl Send for HidDevice {}

impl Debug for HidDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HidDevice").finish()
    }
}

impl Drop for HidDevice {
    fn drop(&mut self) {
        unsafe { ffi::hid_close(self._hid_device) }
    }
}

impl HidDevice {
    /// Check size returned by other methods, if it's equal to -1 check for
    /// error and return Error, otherwise return size as unsigned number
    fn check_size(&self, res: i32) -> HidResult<usize> {
        if res == -1 {
            match self.check_error() {
                Ok(err) => Err(err),
                Err(e) => Err(e),
            }
        } else {
            Ok(res as usize)
        }
    }
}

impl HidDevice {
    fn check_error(&self) -> HidResult<HidError> {
        Ok(HidError::HidApiError {
            message: unsafe {
                match wchar_to_string(ffi::hid_error(self._hid_device)) {
                    WcharString::String(s) => s,
                    _ => return Err(HidError::HidApiErrorEmpty),
                }
            },
        })
    }
}

impl HidDeviceBackendBase for HidDevice {

    fn write(&self, data: &[u8]) -> HidResult<usize> {
        if data.is_empty() {
            return Err(HidError::InvalidZeroSizeData);
        }
        let res = unsafe { ffi::hid_write(self._hid_device, data.as_ptr(), data.len() as size_t) };
        self.check_size(res)
    }

    fn read(&self, buf: &mut [u8]) -> HidResult<usize> {
        let res = unsafe { ffi::hid_read(self._hid_device, buf.as_mut_ptr(), buf.len() as size_t) };
        self.check_size(res)
    }

    fn read_timeout(&self, buf: &mut [u8], timeout: i32) -> HidResult<usize> {
        let res = unsafe {
            ffi::hid_read_timeout(
                self._hid_device,
                buf.as_mut_ptr(),
                buf.len() as size_t,
                timeout,
            )
        };
        self.check_size(res)
    }

    fn send_feature_report(&self, data: &[u8]) -> HidResult<()> {
        if data.is_empty() {
            return Err(HidError::InvalidZeroSizeData);
        }
        let res = unsafe {
            ffi::hid_send_feature_report(self._hid_device, data.as_ptr(), data.len() as size_t)
        };
        let res = self.check_size(res)?;
        if res != data.len() {
            Err(HidError::IncompleteSendError {
                sent: res,
                all: data.len(),
            })
        } else {
            Ok(())
        }
    }

    /// Set the first byte of `buf` to the 'Report ID' of the report to be read.
    /// Upon return, the first byte will still contain the Report ID, and the
    /// report data will start in `buf[1]`.
    fn get_feature_report(&self, buf: &mut [u8]) -> HidResult<usize> {
        let res = unsafe {
            ffi::hid_get_feature_report(self._hid_device, buf.as_mut_ptr(), buf.len() as size_t)
        };
        self.check_size(res)
    }

    fn set_blocking_mode(&self, blocking: bool) -> HidResult<()> {
        let res = unsafe {
            ffi::hid_set_nonblocking(self._hid_device, if blocking { 0i32 } else { 1i32 })
        };
        if res == -1 {
            Err(HidError::SetBlockingModeError {
                mode: match blocking {
                    true => "blocking",
                    false => "not blocking",
                },
            })
        } else {
            Ok(())
        }
    }

    fn get_manufacturer_string(&self) -> HidResult<Option<String>> {
        let mut buf = [0 as wchar_t; STRING_BUF_LEN];
        let res = unsafe {
            ffi::hid_get_manufacturer_string(
                self._hid_device,
                buf.as_mut_ptr(),
                STRING_BUF_LEN as size_t,
            )
        };
        let res = self.check_size(res)?;
        unsafe { Ok(wchar_to_string(buf[..res].as_ptr()).into()) }
    }

    fn get_product_string(&self) -> HidResult<Option<String>> {
        let mut buf = [0 as wchar_t; STRING_BUF_LEN];
        let res = unsafe {
            ffi::hid_get_product_string(
                self._hid_device,
                buf.as_mut_ptr(),
                STRING_BUF_LEN as size_t,
            )
        };
        let res = self.check_size(res)?;
        unsafe { Ok(wchar_to_string(buf[..res].as_ptr()).into()) }
    }

    fn get_serial_number_string(&self) -> HidResult<Option<String>> {
        let mut buf = [0 as wchar_t; STRING_BUF_LEN];
        let res = unsafe {
            ffi::hid_get_serial_number_string(
                self._hid_device,
                buf.as_mut_ptr(),
                STRING_BUF_LEN as size_t,
            )
        };
        let res = self.check_size(res)?;
        unsafe { Ok(wchar_to_string(buf[..res].as_ptr()).into()) }
    }

    fn get_indexed_string(&self, index: i32) -> HidResult<Option<String>> {
        let mut buf = [0 as wchar_t; STRING_BUF_LEN];
        let res = unsafe {
            ffi::hid_get_indexed_string(
                self._hid_device,
                index as c_int,
                buf.as_mut_ptr(),
                STRING_BUF_LEN,
            )
        };
        let res = self.check_size(res)?;
        unsafe { Ok(wchar_to_string(buf[..res].as_ptr()).into()) }
    }

    fn get_device_info(&self) -> HidResult<DeviceInfo> {
        let raw_device = unsafe { ffi::hid_get_device_info(self._hid_device) };
        if raw_device.is_null() {
            match self.check_error() {
                Ok(err) | Err(err) => return Err(err),
            }
        }

        unsafe { conv_hid_device_info(raw_device) }
    }
}

impl HidDeviceBackendWindows for HidDevice {
    fn get_container_id(&self) -> HidResult<GUID> {
        let mut container_id: GUID = unsafe { std::mem::zeroed() };

        let res = unsafe {
            ffi::windows::hid_winapi_get_container_id(self._hid_device, addr_of_mut!(container_id))
        };

        if res == -1 {
            match self.check_error() {
                Ok(err) => Err(err),
                Err(err) => Err(err),
            }
        } else {
            Ok(container_id)
        }
    }
}


#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum InternalBuyType {
    Unknown,
    Usb,
    Bluetooth,
    BluetoothLE,
    I2c,
    Spi,
}

impl From<InternalBuyType> for BusType {
    fn from(value: InternalBuyType) -> Self {
        match value {
            InternalBuyType::Unknown => BusType::Unknown,
            InternalBuyType::Usb => BusType::Usb,
            InternalBuyType::Bluetooth => BusType::Bluetooth,
            InternalBuyType::BluetoothLE => BusType::Bluetooth,
            InternalBuyType::I2c => BusType::I2c,
            InternalBuyType::Spi => BusType::Spi
        }
    }
}


fn to_upper(u16str: &mut [u16]) {
    for c in u16str {
        if let Ok(t) = u8::try_from(*c) {
            *c = t.to_ascii_uppercase().into();
        }
    }
}

fn find_first_upper_case(u16str: &[u16], pattern: &str) -> Option<usize> {
    u16str
        .windows(pattern.encode_utf16().count())
        .enumerate()
        .filter(|(_, ss)| ss
            .iter()
            .copied()
            .zip(pattern.encode_utf16())
            .all(|(l, r)| l == r))
        .map(|(i, _)| i)
        .next()
}

fn starts_with_ignore_case(utf16str: &[u16], pattern: &str) -> bool {
    //The hidapi c library uses `contains` instead of `starts_with`,
    // but as far as I can tell `starts_with` is a better choice
    char::decode_utf16(utf16str.iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .zip(pattern.chars())
        .all(|(l, r)| l.eq_ignore_ascii_case(&r))
}

fn extract_int_token_value(u16str: &[u16], token: &str) -> Option<u32> {
    let start = find_first_upper_case(u16str, token)? + token.encode_utf16().count();
    char::decode_utf16(u16str[start..].iter().copied())
        .map_while(|c| c
            .ok()
            .and_then(|c| c.to_digit(16)))
        .reduce(|l, r| l * 16 + r)
}

fn u16str_to_wstring(u16str: &[u16]) -> WcharString {
    String::from_utf16(u16str)
        .map(WcharString::String)
        .unwrap_or_else(|_| WcharString::Raw(u16str.to_vec()))
}


fn get_device_interface_property(interface_path: PCWSTR, property_key: &DEVPROPKEY, expected_property_type: DEVPROPTYPE) -> Option<Vec<u8>> {
    let mut property_type = 0;
    let mut len = 0;
    let cr = unsafe {
        CM_Get_Device_Interface_PropertyW(
            interface_path,
            property_key,
            &mut property_type,
            null_mut(),
            &mut len,
            0
        )
    };
    ensure!(cr == CR_BUFFER_SMALL && property_type == expected_property_type, None);
    let mut property_value = vec![0u8; len as usize];
    let cr = unsafe {
        CM_Get_Device_Interface_PropertyW(
            interface_path,
            property_key,
            &mut property_type,
            property_value.as_mut_ptr(),
            &mut len,
            0
        )
    };
    assert_eq!(property_value.len(), len as usize);
    ensure!(cr == CR_SUCCESS, None);
    Some(property_value)
}

fn get_devnode_property(dev_node: u32, property_key: *const DEVPROPKEY, expected_property_type: DEVPROPTYPE) -> Option<Vec<u8>> {
    let mut property_type = 0;
    let mut len = 0;
    let cr = unsafe {
        CM_Get_DevNode_PropertyW(
            dev_node,
            property_key,
            &mut property_type,
            null_mut(),
            &mut len,
            0
        )
    };
    ensure!(cr == CR_BUFFER_SMALL && property_type == expected_property_type, None);
    let mut property_value = vec![0u8; len as usize];
    let cr = unsafe {
        CM_Get_DevNode_PropertyW(
            dev_node,
            property_key,
            &mut property_type,
            property_value.as_mut_ptr(),
            &mut len,
            0
        )
    };
    assert_eq!(property_value.len(), len as usize);
    ensure!(cr == CR_SUCCESS, None);
    Some(property_value)
}

fn get_dev_node_parent(dev_node: u32) -> Option<u32> {
    let mut parent = 0;
    match unsafe { CM_Get_Parent(&mut parent, dev_node, 0)} {
        CR_SUCCESS => Some(parent),
        _ => None
    }
}
