use core::cmp::min;
use core::mem;
use crate::{Result, UsbDirection, UsbError};
use crate::bus::UsbBus;
use crate::control::Request;
use crate::endpoint::{EndpointIn, EndpointOut};

#[derive(Debug)]
#[allow(unused)]
enum ControlState {
    Idle,
    DataIn,
    DataInZlp,
    DataInLast,
    CompleteIn(Request),
    StatusOut,
    CompleteOut,
    DataOut(Request),
    StatusIn,
    Error,
}

// Maximum length of control transfer data stage in bytes. It might be necessary to make this
// non-const in the future.
const CONTROL_BUF_LEN: usize = 256;

/// Buffers and parses USB control transfers.
pub struct ControlPipe<'a, B: UsbBus> {
    ep_out: EndpointOut<'a, B>,
    ep_in: EndpointIn<'a, B>,
    state: ControlState,
    buf: [u8; CONTROL_BUF_LEN],
    i: usize,
    len: usize,
}

impl<B: UsbBus> ControlPipe<'_, B> {
    pub fn new<'a>(ep_out: EndpointOut<'a, B>, ep_in: EndpointIn<'a, B>) -> ControlPipe<'a, B> {
        ControlPipe {
            ep_out,
            ep_in,
            state: ControlState::Idle,
            buf: unsafe { mem::uninitialized() },
            i: 0,
            len: 0,
        }
    }

    pub fn waiting_for_response(&self) -> bool {
        match self.state {
            ControlState::CompleteOut | ControlState::CompleteIn(_) => true,
            _ => false,
        }
    }

    pub fn data(&self) -> &[u8] {
        &self.buf[0..self.len]
    }

    pub fn reset(&mut self) {
        self.state = ControlState::Idle;
    }

    pub fn handle_setup<'p>(&'p mut self) -> Option<Request> {
        let count = match self.ep_out.read(&mut self.buf[..]) {
            Ok(count) => count,
            Err(UsbError::WouldBlock) => return None,
            Err(_) => {
                self.set_error();
                return None;
            }
        };

        let req = match Request::parse(&self.buf[0..count]) {
            Ok(req) => req,
            Err(_) => {
                // Failed to parse SETUP packet
                self.set_error();
                return None;
            },
        };

        /*sprintln!("SETUP {:?} {:?} {:?} req:{} val:{} idx:{} len:{} {:?}",
            req.direction, req.request_type, req.recipient,
            req.request, req.value, req.index, req.length,
            self.state);*/

        if req.direction == UsbDirection::Out {
            // OUT transfer

            if req.length > 0 {
                // Has data stage

                if req.length as usize > self.buf.len() {
                    // Data stage won't fit in buffer
                    self.set_error();
                    return None;
                }

                self.i = 0;
                self.len = req.length as usize;
                self.state = ControlState::DataOut(req);
            } else {
                // No data stage

                self.len = 0;
                self.state = ControlState::CompleteOut;
                return Some(req);
            }
        } else {
            // IN transfer

            self.state = ControlState::CompleteIn(req);
            return Some(req);
        }

        return None;
    }

    pub fn handle_out<'p>(&'p mut self) -> Option<Request> {
        match self.state {
            ControlState::DataOut(req) => {
                let i = self.i;
                let count = match self.ep_out.read(&mut self.buf[i..]) {
                    Ok(count) => count,
                    Err(UsbError::WouldBlock) => return None,
                    Err(_) => {
                        // Failed to read or buffer overflow (overflow is only possible if the host
                        // sends more data than it indicated in the SETUP request)
                        self.set_error();
                        return None;
                    },
                };

                self.i += count;

                if self.i >= self.len {
                    self.state = ControlState::CompleteOut;
                    return Some(req);
                }
            },
            ControlState::StatusOut => {
                self.ep_out.read(&mut []).ok();
                self.state = ControlState::Idle;
            },
            _ => {
                // Discard the packet
                self.ep_out.read(&mut []).ok();

                // Unexpected OUT packet
                self.set_error()
            },
        }

        return None;
    }

    pub fn handle_in_complete(&mut self) -> bool {
        match self.state {
            ControlState::DataIn => {
                self.write_in_chunk();
            },
            ControlState::DataInZlp => {
                if self.ep_in.write(&[]).is_err() {
                    // There isn't much we can do if the write fails, except to wait for another
                    // poll or for the host to resend the request.
                    return false;
                }

                self.state = ControlState::DataInLast;
            },
            ControlState::DataInLast => {
                self.ep_out.unstall();
                self.state = ControlState::StatusOut;
            },
            ControlState::StatusIn => {
                self.state = ControlState::Idle;
                return true;
            },
            _ => {
                // Unexpected IN packet
                self.set_error();
            }
        };

        return false;
    }

    fn write_in_chunk(&mut self) {
        let count = min(self.len - self.i, self.ep_in.max_packet_size() as usize);

        let count = match self.ep_in.write(&self.buf[self.i..(self.i+count)]) {
            Ok(c) => c,
            // There isn't much we can do if the write fails, except to wait for another poll or for
            // the host to resend the request.
            Err(_) => return,
        };

        self.i += count;

        if self.i >= self.len {
            self.state = if count == self.ep_in.max_packet_size() as usize {
                ControlState::DataInZlp
            } else {
                ControlState::DataInLast
            };
        }
    }

    pub fn accept_out(&mut self) -> Result<()> {
        match self.state {
            ControlState::CompleteOut => {},
            _ => return Err(UsbError::InvalidState),
        };

        self.ep_in.write(&[]).ok();
        self.state = ControlState::StatusIn;
        Ok(())
    }

    pub fn accept_in(&mut self, f: impl FnOnce(&mut [u8]) -> Result<usize>) -> Result<()> {
        let req = match self.state {
            ControlState::CompleteIn(req) => req,
            _ => return Err(UsbError::InvalidState),
        };

        let len = f(&mut self.buf[..])?;

        if len > self.buf.len() {
            self.set_error();
            return Err(UsbError::BufferOverflow);
        }

        self.len = min(len, req.length as usize);
        self.i = 0;
        self.state = ControlState::DataIn;
        self.write_in_chunk();

        Ok(())
    }

    pub fn reject(&mut self) -> Result<()> {
        if !self.waiting_for_response() {
            return Err(UsbError::InvalidState);
        }

        self.set_error();
        Ok(())
    }

    fn set_error(&mut self) {
        self.state = ControlState::Error;
        self.ep_out.stall();
        self.ep_in.stall();
    }
}
