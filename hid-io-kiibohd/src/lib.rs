/* Copyright (C) 2021 by Jacob Alexander
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */

// ----- Modules -----

#![no_std]

// ----- Crates -----

use core::convert::TryFrom;
use core::ptr::copy_nonoverlapping;
use cstr_core::c_char;
use cstr_core::CStr;
use heapless::consts::{U10, U256, U276, U64, U8};
use heapless::{ArrayLength, String, Vec};
use hid_io_protocol::commands::*;
use hid_io_protocol::*;
use pkg_version::*;
use typenum::Unsigned;

// ----- Types -----

type BufChunk = U64;
type IdLen = U10;
type MessageLen = U256;
type RxBuf = U8;
type SerializationLen = U276;
type TxBuf = U8;

// ----- Globals -----

static mut INTF: Option<
    CommandInterface<TxBuf, RxBuf, BufChunk, MessageLen, SerializationLen, IdLen>,
> = None;

// ----- External C Callbacks -----

extern "C" {
    /// h0016 callback for Flash Mode
    ///
    /// val (output)
    /// - Scancode if Ack
    /// - Errorcode if Nak
    ///
    /// Return:
    /// - true (Ack)
    /// - false (Nak)
    fn h0016_flashmode_cmd(val: *mut u16) -> bool;

    /// h001a callback for Sleep Mode
    ///
    /// val (output)
    /// - Unused if Ack
    /// - Errorcode if Nak
    ///
    /// Return:
    /// - true (Ack)
    /// - false (Nak)
    fn h001a_sleepmode_cmd(val: *mut u8) -> bool;

    /// h0031 callback for Terminal Command
    /// Used for both ack and noack versions of command.
    /// Nothing changes for the callback in either case.
    ///
    /// string (input)
    /// - String used to call terminal command
    ///
    /// len (input)
    /// - Length of string in bytes
    ///
    /// noack (input)
    /// - Whether this is a no-ack command
    /// - Return value changes in noack mode
    ///
    /// Return (ack)
    /// - true (Ack)
    /// - false (NaK)
    ///
    /// Return (noack)
    /// - true (Success)
    /// - false (error condition for hid-io-protocol)
    fn h0031_terminalcmd_cmd(string: *const c_char, len: u16) -> bool;

    /// h0050 callback for Manufacturing tests
    ///
    /// command (input)
    /// - Manufacturing command to run
    /// argument (input)
    /// - Argument to manufacturing command
    ///
    /// data (output)
    /// - Byte array of data to send back to caller
    ///   (ack or nak)
    /// len (input)
    /// - Length of provided memory (data)
    ///
    /// Return:
    /// - true (Ack)
    /// - false (Nak)
    fn h0050_manufacturing_cmd(command: u16, argument: u16, data: *mut u8, len: u16) -> bool;
}

// ----- External C Interface -----

#[repr(C)]
pub enum HidioStatus {
    Success,
    BufferEmpty,
    BufferNotReady,
    ErrorIdVecTooSmall,
    ErrorInvalidUTF8,
    ErrorBufSizeTooLarge,
    ErrorBufSizeTooSmall,
    ErrorBufFull,
    ErrorDetailed,
    ErrorNotInitialized,
}

#[repr(C)]
pub struct HidioConfig {
    device_name: *const c_char,
    device_serial_number: *const c_char,
    device_mcu: *const c_char,
    firmware_version: *const c_char,
    firmware_vendor: *const c_char,
}

#[repr(C)]
#[derive(Clone)]
pub struct HidioHostInfo {
    major_version: u16,
    minor_version: u16,
    patch_version: u16,
    os: u8,
    os_version: *const c_char,
    host_software_name: *const c_char,
}

/// Most recent detailed error
/// Valid whenever HidioStatus_ErrorDetailed* is returned.
#[no_mangle]
pub extern "C" fn hidio_error() -> *const c_char {
    unsafe {
        // Retrieve interface
        let intf = match INTF.as_mut() {
            Some(intf) => intf,
            None => {
                return CStr::from_bytes_with_nul(b"Not initialized\0")
                    .unwrap()
                    .as_ptr()
            }
        };

        match CStr::from_bytes_with_nul(intf.error_str.as_bytes()) {
            Ok(cstr) => cstr,
            Err(_) => CStr::from_bytes_with_nul(b"Invalid error\0").unwrap(),
        }
        .as_ptr()
    }
}

/// Size of each hid-io buffer chunk
/// This is the transmission length of the serialized packet
#[no_mangle]
pub extern "C" fn hidio_bufchunk_size() -> u16 {
    <BufChunk as Unsigned>::to_u16()
}

/// Size of hid-io rx buffer (in multiples of hidio_bufchunk_size())
#[no_mangle]
pub extern "C" fn hidio_rxbyte_bufsize() -> u16 {
    <RxBuf as Unsigned>::to_u16()
}

/// Size of hid-io tx buffer (in multiples of hidio_bufchunk_size())
#[no_mangle]
pub extern "C" fn hidio_txbyte_bufsize() -> u16 {
    <TxBuf as Unsigned>::to_u16()
}

/// Initialized the hid-io CommandInterface
#[no_mangle]
pub extern "C" fn hidio_init(config: HidioConfig) -> HidioStatus {
    let ids = [
        HidIoCommandID::SupportedIDs,
        HidIoCommandID::GetInfo,
        HidIoCommandID::TestPacket,
        HidIoCommandID::ManufacturingTest,
    ];

    unsafe {
        INTF = Some(match CommandInterface::<TxBuf, RxBuf, BufChunk, MessageLen, SerializationLen, IdLen>::new(&ids, config) {
            Ok(intf) => intf,
            Err(CommandError::IdVecTooSmall) => {
                return HidioStatus::ErrorIdVecTooSmall;
            }
            Err(_) => {
                // TODO Convert errors properly
                //HIDIO_ERROR.copy_from_slice("ERROR TODO".as_bytes());
                return HidioStatus::ErrorDetailed;
            }
        });
    }
    HidioStatus::Success
}

/// # Safety
/// Takes in a byte array of the given length and adds it to the
/// hid-io rx byte processing buffer as a chunk.
/// Cannot be larger than the configured max chunk size.
#[no_mangle]
pub unsafe extern "C" fn hidio_rx_bytes(bytes: *const u8, len: u16) -> HidioStatus {
    // Make sure the incoming buffer is a valid size
    if len > <RxBuf as Unsigned>::to_u16() {
        return HidioStatus::ErrorBufSizeTooLarge;
    }

    // Copy into rx buffer
    let slice = core::slice::from_raw_parts(bytes, len as usize);

    // Get rx_bytebuf
    let rx_bytebuf = match INTF.as_mut() {
        Some(intf) => &mut intf.rx_bytebuf,
        None => {
            return HidioStatus::ErrorNotInitialized;
        }
    };

    // Enqueue bytes into buffer
    match rx_bytebuf.enqueue(match Vec::from_slice(slice) {
        Ok(vec) => vec,
        Err(_) => {
            return HidioStatus::ErrorBufSizeTooSmall;
        }
    }) {
        Ok(_) => HidioStatus::Success,
        Err(_) => HidioStatus::ErrorBufFull,
    }
}

/// # Safety
/// Takes a block of memory (defined by len) and writes to it from the
/// hid-io tx byte buffer. If written successfully the bytes
/// are dequeued from the tx byte buffer.
/// The chunk len buf must equal HID_IO_BUF_CHUNK_SIZE.
#[no_mangle]
pub unsafe extern "C" fn hidio_tx_bytes(bytes: *mut u8, len: u16) -> HidioStatus {
    // Make sure the buffer is the correct size
    if len > <TxBuf as Unsigned>::to_u16() {
        return HidioStatus::ErrorBufSizeTooLarge;
    }
    if len < <TxBuf as Unsigned>::to_u16() {
        return HidioStatus::ErrorBufSizeTooSmall;
    }

    // Retrieve tx_buf
    let tx_bytebuf = match INTF.as_mut() {
        Some(intf) => &mut intf.tx_bytebuf,
        None => {
            return HidioStatus::ErrorNotInitialized;
        }
    };

    // Copy a single chunk from the tx_buffer
    match tx_bytebuf.dequeue() {
        Some(chunk) => {
            copy_nonoverlapping(chunk.as_ptr(), bytes, len as usize);
            HidioStatus::Success
        }
        None => HidioStatus::BufferEmpty,
    }
}

/// Reads from the pending hid-io rx byte buffer, builds a hid-io
/// message buffer, handles the message, calls any implemented
/// callbacks, then sends the reply (if applicable) to the hid-io tx
/// byte buffer.
///
/// count defines the maximum number of buffers to process.
/// Setting to 0 processes buffers until the rx byte buffer is empty.
#[no_mangle]
pub extern "C" fn hidio_rx_process(count: u8) -> HidioStatus {
    unsafe {
        // Retrieve interface
        let intf = match INTF.as_mut() {
            Some(intf) => intf,
            None => {
                return HidioStatus::ErrorNotInitialized;
            }
        };

        // Process buffers
        match intf.process_rx(count) {
            Ok(processed) => {
                if processed > 0 {
                    HidioStatus::Success
                } else {
                    HidioStatus::BufferNotReady
                }
            }
            Err(_) => {
                // TODO Convert errors properly
                //HIDIO_ERROR.copy_from_slice("ERROR TODO".as_bytes());
                HidioStatus::ErrorDetailed
            }
        }
    }
}

// ----- External Command Interface -----

/// Queries useful information from hid-io-core
#[no_mangle]
pub extern "C" fn hidio_h0001_info() -> HidioStatus {
    use h0001::*;

    // Useful hid-io-core properties
    let properties = [
        Property::MajorVersion,
        Property::MinorVersion,
        Property::PatchVersion,
        Property::OsType,
        Property::OsVersion,
        Property::HostSoftwareName,
    ];

    unsafe {
        // Retrieve interface
        let intf = match INTF.as_mut() {
            Some(intf) => intf,
            None => {
                return HidioStatus::ErrorNotInitialized;
            }
        };

        // Send requests for useful properties
        for property in &properties {
            if intf
                .h0001_info(Cmd {
                    property: *property,
                })
                .is_err()
            {
                // TODO Better error handling
                return HidioStatus::ErrorDetailed;
            }
        }
    }

    HidioStatus::Success
}

/// # Safety
/// Get stored hid-io-core information
/// May not be complete if a response has not been retrieved
/// Returns True if data is valid
/// Returns False if not ready
#[no_mangle]
pub unsafe extern "C" fn h0001_info_data(info: *mut HidioHostInfo) -> bool {
    // Retrieve interface
    let intf = match INTF.as_mut() {
        Some(intf) => intf,
        None => {
            return false;
        }
    };

    *info = intf.hostinfo.clone();
    true
}

/// # Safety
/// Send UTF-8 string to hid-io to be printed
/// Sent as no-ack so there is no response whether the command worked
/// Expects a null-terminated UTF-8 string
#[no_mangle]
pub unsafe extern "C" fn hidio_h0017_unicodetext(string: *const c_char) -> HidioStatus {
    use h0017::*;

    // Retrieve interface
    let intf = match INTF.as_mut() {
        Some(intf) => intf,
        None => {
            return HidioStatus::ErrorNotInitialized;
        }
    };

    // Prepare UTF-8 string
    let cstr = CStr::from_ptr(string);
    let utf8string = match cstr.to_str() {
        Ok(utf8string) => utf8string,
        Err(_) => {
            return HidioStatus::ErrorInvalidUTF8;
        }
    };

    // Send command
    if intf
        .h0017_unicodetext(
            Cmd {
                string: String::from(utf8string),
            },
            true,
        )
        .is_err()
    {
        // TODO Better error handling
        return HidioStatus::ErrorDetailed;
    }

    HidioStatus::Success
}

/// # Safety
/// Sends a list of UTF-8 symbols to have "held".
/// To release the symbols send another list without those symbols present.
/// Sent as no-ack so there is no response whether the command worked
/// Expects a null-terminated UTF-8 string
///
/// Will only return false on some early failures as this is a no-ack command
/// (invalid UTF-8)
#[no_mangle]
pub unsafe extern "C" fn hidio_h0018_unicodestate(symbols: *const c_char) -> HidioStatus {
    use h0018::*;

    // Retrieve interface
    let intf = match INTF.as_mut() {
        Some(intf) => intf,
        None => {
            return HidioStatus::ErrorNotInitialized;
        }
    };

    // Prepare UTF-8 string
    let cstr = CStr::from_ptr(symbols);
    let utf8string = match cstr.to_str() {
        Ok(utf8string) => utf8string,
        Err(_) => {
            return HidioStatus::ErrorInvalidUTF8;
        }
    };

    // Send command
    if intf
        .h0018_unicodestate(
            Cmd {
                symbols: String::from(utf8string),
            },
            true,
        )
        .is_err()
    {
        // TODO Better error handling
        return HidioStatus::ErrorDetailed;
    }

    HidioStatus::Success
}

/// # Safety
/// Sends terminal output to the host
/// Sent as no-ack so there is no response whether the command worked
/// Expects a null-terminated UTF-8 string
///
/// Will only return false on some early failures as this is a no-ack command
/// (invalid UTF-8)
#[no_mangle]
pub unsafe extern "C" fn hidio_h0034_terminalout(output: *const c_char) -> HidioStatus {
    use h0034::*;

    // Retrieve interface
    let intf = match INTF.as_mut() {
        Some(intf) => intf,
        None => {
            return HidioStatus::ErrorNotInitialized;
        }
    };

    // Prepare UTF-8 string
    let cstr = CStr::from_ptr(output);
    let utf8string = match cstr.to_str() {
        Ok(utf8string) => utf8string,
        Err(_) => {
            return HidioStatus::ErrorInvalidUTF8;
        }
    };

    // Send command
    if intf
        .h0034_terminalout(
            Cmd {
                output: String::from(utf8string),
            },
            true,
        )
        .is_err()
    {
        // TODO Better error handling
        return HidioStatus::ErrorDetailed;
    }

    HidioStatus::Success
}

// ----- Command Interface -----

struct CommandInterface<
    TX: ArrayLength<Vec<u8, N>>,
    RX: ArrayLength<Vec<u8, N>>,
    N: ArrayLength<u8>,
    H: ArrayLength<u8>,
    S: ArrayLength<u8>,
    ID: ArrayLength<HidIoCommandID> + ArrayLength<u8>,
> where
    H: core::fmt::Debug,
    H: Sub<B1>,
{
    ids: Vec<HidIoCommandID, ID>,
    rx_bytebuf: buffer::Buffer<RX, N>,
    rx_packetbuf: HidIoPacketBuffer<H>,
    tx_bytebuf: buffer::Buffer<TX, N>,
    serial_buf: Vec<u8, S>,
    config: HidioConfig,
    hostinfo: HidioHostInfo,
    error_str: String<U256>,
    os_version: String<U256>,
    host_software_name: String<U256>,
}

impl<
        TX: ArrayLength<Vec<u8, N>>,
        RX: ArrayLength<Vec<u8, N>>,
        N: ArrayLength<u8>,
        H: ArrayLength<u8>,
        S: ArrayLength<u8>,
        ID: ArrayLength<HidIoCommandID> + ArrayLength<u8>,
    > CommandInterface<TX, RX, N, H, S, ID>
where
    H: core::fmt::Debug,
    H: Sub<B1>,
{
    fn new(
        ids: &[HidIoCommandID],
        config: HidioConfig,
    ) -> Result<CommandInterface<TX, RX, N, H, S, ID>, CommandError> {
        // Make sure we have a large enough id vec
        let ids = match Vec::from_slice(ids) {
            Ok(ids) => ids,
            Err(_) => {
                return Err(CommandError::IdVecTooSmall);
            }
        };

        let os_version = String::new();
        let host_software_name = String::new();

        let tx_bytebuf = buffer::Buffer::new();
        let rx_bytebuf = buffer::Buffer::new();
        let rx_packetbuf = HidIoPacketBuffer::new();
        let serial_buf = Vec::new();
        let error_str = String::new();
        let hostinfo = HidioHostInfo {
            major_version: 0,
            minor_version: 0,
            patch_version: 0,
            os: 0,
            os_version: os_version.as_ptr() as *const c_char,
            host_software_name: host_software_name.as_ptr() as *const c_char,
        };

        Ok(CommandInterface {
            ids,
            rx_bytebuf,
            rx_packetbuf,
            tx_bytebuf,
            serial_buf,
            config,
            error_str,
            hostinfo,
            os_version,
            host_software_name,
        })
    }

    /// Decode rx_bytebuf into a HidIoPacketBuffer
    /// Returns true if buffer ready, false if not
    fn rx_packetbuffer_decode(&mut self) -> Result<bool, CommandError> {
        loop {
            // Retrieve vec chunk
            if let Some(buf) = self.rx_bytebuf.dequeue() {
                // Decode chunk
                match self.rx_packetbuf.decode_packet(&buf) {
                    Ok(_recv) => {
                        // Only handle buffer if ready
                        if self.rx_packetbuf.done {
                            // Handle sync packet type
                            match self.rx_packetbuf.ptype {
                                HidIoPacketType::Sync => {
                                    self.rx_packetbuf.clear();
                                }
                                _ => {
                                    return Ok(true);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        return Err(CommandError::PacketDecodeError(e));
                    }
                }
            } else {
                return Ok(false);
            }
        }
    }

    /// Process rx buffer until empty
    /// Handles flushing tx->rx, decoding, then processing buffers
    /// Returns the number of buffers processed
    fn process_rx(&mut self, count: u8) -> Result<u8, CommandError>
    where
        <H as Sub<B1>>::Output: ArrayLength<u8>,
    {
        // Decode bytes into buffer
        let mut cur = 0;
        while (count == 0 || cur < count) && self.rx_packetbuffer_decode()? {
            // Process rx buffer
            self.rx_message_handling(self.rx_packetbuf.clone())?;

            // Clear buffer
            self.rx_packetbuf.clear();
            cur += 1;
        }

        Ok(cur)
    }
}

/// CommandInterface for Commands
/// TX - tx byte buffer size (in multiples of N)
/// RX - tx byte buffer size (in multiples of N)
/// N - Max payload length (HidIoPacketBuffer), used for default values
/// H - Max data payload length (HidIoPacketBuffer)
/// S - Serialization buffer size
/// ID - Max number of HidIoCommandIDs
impl<
        TX: ArrayLength<Vec<u8, N>>,
        RX: ArrayLength<Vec<u8, N>>,
        N: ArrayLength<u8>,
        H: ArrayLength<u8>,
        S: ArrayLength<u8>,
        ID: ArrayLength<HidIoCommandID> + ArrayLength<u8>,
    > Commands<H, ID> for CommandInterface<TX, RX, N, H, S, ID>
where
    H: core::fmt::Debug + Sub<B1>,
{
    fn default_packet_chunk(&self) -> u32 {
        <N as Unsigned>::to_u32()
    }

    fn tx_packetbuffer_send(&mut self, buf: &mut HidIoPacketBuffer<H>) -> Result<(), CommandError> {
        let size = buf.serialized_len() as usize;
        if self.serial_buf.resize_default(size).is_err() {
            return Err(CommandError::SerializationVecTooSmall);
        }
        let data = match buf.serialize_buffer(&mut self.serial_buf) {
            Ok(data) => data,
            Err(err) => {
                return Err(CommandError::SerializationFailed(err));
            }
        };

        // Add serialized data to buffer
        // May need to enqueue multiple packets depending how much
        // was serialized
        for pos in (0..data.len()).step_by(<N as Unsigned>::to_usize()) {
            let len = core::cmp::min(<N as Unsigned>::to_usize(), data.len() - pos);
            match self
                .tx_bytebuf
                .enqueue(match Vec::from_slice(&data[pos..len + pos]) {
                    Ok(vec) => vec,
                    Err(_) => {
                        return Err(CommandError::TxBufferVecTooSmall);
                    }
                }) {
                Ok(_) => {}
                Err(_) => {
                    return Err(CommandError::TxBufferSendFailed);
                }
            }
        }
        Ok(())
    }
    fn supported_id(&self, id: HidIoCommandID) -> bool {
        self.ids.iter().any(|&i| i == id)
    }

    fn h0000_supported_ids_cmd(&mut self, _data: h0000::Cmd) -> Result<h0000::Ack<ID>, h0000::Nak> {
        // Build id list to send back
        Ok(h0000::Ack::<ID> {
            ids: self.ids.clone(),
        })
    }

    /// Uses the CommandInterface to send data directly
    fn h0001_info_cmd(&mut self, data: h0001::Cmd) -> Result<h0001::Ack<Sub1<H>>, h0001::Nak>
    where
        <H as Sub<B1>>::Output: ArrayLength<u8>,
    {
        use h0001::*;

        let property = data.property;
        let os = OSType::Unknown;
        let mut number = 0;
        let mut string = String::new();

        match property {
            Property::MajorVersion => {
                number = pkg_version_major!();
            }
            Property::MinorVersion => {
                number = pkg_version_minor!();
            }
            Property::PatchVersion => {
                number = pkg_version_patch!();
            }
            Property::DeviceName => {
                unsafe {
                    if let Ok(cstr) = CStr::from_ptr(self.config.device_name).to_str() {
                        string = String::from(cstr);
                    }
                };
            }
            Property::DeviceSerialNumber => {
                unsafe {
                    if let Ok(cstr) = CStr::from_ptr(self.config.device_serial_number).to_str() {
                        string = String::from(cstr);
                    }
                };
            }
            Property::DeviceMCU => {
                unsafe {
                    if let Ok(cstr) = CStr::from_ptr(self.config.device_mcu).to_str() {
                        string = String::from(cstr);
                    }
                };
            }
            Property::FirmwareName => {
                string = String::from("kiibohd");
            }
            Property::FirmwareVersion => {
                unsafe {
                    if let Ok(cstr) = CStr::from_ptr(self.config.firmware_version).to_str() {
                        string = String::from(cstr);
                    }
                };
            }
            Property::DeviceVendor => {
                unsafe {
                    if let Ok(cstr) = CStr::from_ptr(self.config.firmware_vendor).to_str() {
                        string = String::from(cstr);
                    }
                };
            }
            _ => {
                return Err(Nak { property });
            }
        }

        Ok(Ack {
            property,
            os,
            number,
            string,
        })
    }
    /// Uses the CommandInterface to store data rather than issue
    /// a callback
    fn h0001_info_ack(&mut self, data: h0001::Ack<Sub1<H>>) -> Result<(), CommandError>
    where
        <H as Sub<B1>>::Output: ArrayLength<u8>,
    {
        use h0001::*;

        match data.property {
            Property::MajorVersion => {
                self.hostinfo.major_version = data.number;
            }
            Property::MinorVersion => {
                self.hostinfo.minor_version = data.number;
            }
            Property::PatchVersion => {
                self.hostinfo.patch_version = data.number;
            }
            Property::OsType => {
                self.hostinfo.os = data.os as u8;
            }
            Property::OsVersion => {
                self.os_version = String::from(data.string.as_str());
            }
            Property::HostSoftwareName => {
                self.host_software_name = String::from(data.string.as_str());
            }
            _ => {
                return Err(CommandError::InvalidProperty8(data.property as u8));
            }
        }

        Ok(())
    }

    fn h0002_test_cmd(&mut self, data: h0002::Cmd<H>) -> Result<h0002::Ack<H>, h0002::Nak> {
        Ok(h0002::Ack { data: data.data })
    }

    fn h0016_flashmode_cmd(&mut self, _data: h0016::Cmd) -> Result<h0016::Ack, h0016::Nak> {
        let mut val = 0;
        if unsafe { h0016_flashmode_cmd(&mut val) } {
            Ok(h0016::Ack { scancode: val })
        } else {
            Err(h0016::Nak {
                error: h0016::Error::try_from(val as u8).unwrap(),
            })
        }
    }

    fn h001a_sleepmode_cmd(&mut self, _data: h001a::Cmd) -> Result<h001a::Ack, h001a::Nak> {
        let mut val = 0;
        if unsafe { h001a_sleepmode_cmd(&mut val) } {
            Ok(h001a::Ack {})
        } else {
            Err(h001a::Nak {
                error: h001a::Error::try_from(val).unwrap(),
            })
        }
    }

    fn h0031_terminalcmd_cmd(&mut self, mut data: h0031::Cmd<H>) -> Result<h0031::Ack, h0031::Nak> {
        // Add null required for CStr
        // This will fail if the command is the max size
        if data.command.push('\0').is_err() {
            return Err(h0031::Nak {});
        }

        // XXX (HaaTa): Yes this will not work for non-ASCII, but that's
        // fine in this context.
        let cstr = match CStr::from_bytes_with_nul(data.command.as_bytes()) {
            Ok(cstr) => cstr,
            Err(_) => {
                return Err(h0031::Nak {});
            }
        };

        if unsafe { h0031_terminalcmd_cmd(cstr.as_ptr(), data.command.len() as u16) } {
            Ok(h0031::Ack {})
        } else {
            Err(h0031::Nak {})
        }
    }
    fn h0031_terminalcmd_nacmd(&mut self, mut data: h0031::Cmd<H>) -> Result<(), CommandError> {
        // Add null required for CStr
        // This will fail if the command is the max size
        if data.command.push('\0').is_err() {
            return Err(CommandError::DataVecTooSmall);
        }

        // XXX (HaaTa): Yes this will not work for non-ASCII, but that's
        // fine in this context.
        let cstr = match CStr::from_bytes_with_nul(data.command.as_bytes()) {
            Ok(cstr) => cstr,
            Err(_) => {
                return Err(CommandError::InvalidCStr);
            }
        };

        if unsafe { h0031_terminalcmd_cmd(cstr.as_ptr(), data.command.len() as u16) } {
            Ok(())
        } else {
            Err(CommandError::CallbackFailed)
        }
    }

    fn h0050_manufacturing_cmd(
        &mut self,
        data: h0050::Cmd,
    ) -> Result<h0050::Ack<H>, h0050::Nak<H>> {
        // Prepare data buffer for manufacturing test results
        let mut databuf = Vec::new();
        if databuf.resize_default(<H as Unsigned>::to_usize()).is_err() {
            return Err(h0050::Nak { data: databuf });
        }

        // Callback
        unsafe {
            if h0050_manufacturing_cmd(
                data.command,
                data.argument,
                databuf.as_mut_ptr(),
                databuf.len() as u16,
            ) {
                Ok(h0050::Ack { data: databuf })
            } else {
                Err(h0050::Nak { data: databuf })
            }
        }
    }
}
