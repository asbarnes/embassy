#![macro_use]
#![allow(missing_docs)]
use core::future::poll_fn;
use core::marker::PhantomData;
use core::task::Poll;

use embassy_hal_internal::into_ref;
use embassy_sync::waitqueue::AtomicWaker;
use embassy_time::{Duration, Instant, Timer};
use embassy_usb_driver::host::{
    ChannelError, ChannelIn, ChannelOut, EndpointDescriptor, TransferOptions, USBHostDriverTrait,
};
use embassy_usb_driver::EndpointType;
use stm32_metapac::common::{Reg, RW};
use stm32_metapac::usb::regs::Epr;

use super::{DmPin, DpPin, Instance};
use crate::pac::usb::regs;
use crate::pac::usb::vals::{EpType, Stat};
use crate::pac::USBRAM;
use crate::{interrupt, Peripheral};

/// Interrupt handler.
pub struct USBHostInterruptHandler<T: Instance> {
    _phantom: PhantomData<T>,
}

impl<T: Instance> interrupt::typelevel::Handler<T::Interrupt> for USBHostInterruptHandler<T> {
    unsafe fn on_interrupt() {
        let regs = T::regs();
        // let x = regs.istr().read().0;
        // trace!("USB IRQ: {:08x}", x);

        let mut int_cleared = false;

        let istr = regs.istr().read();

        // Detect device connect/disconnect
        if istr.reset() {
            trace!("USB IRQ: device connect/disconnect");

            // Write 0 to clear.
            let mut clear = regs::Istr(!0);
            clear.set_reset(false);
            regs.istr().write_value(clear);

            // Wake main thread.
            BUS_WAKER.wake();

            int_cleared = true;
        }

        if istr.ctr() {
            let index = istr.ep_id() as usize;

            let epr = regs.epr(index).read();

            let mut epr_value = invariant(epr);
            // Check and clear error flags
            if epr.err_tx() {
                epr_value.set_err_tx(false);
                warn!("err_tx");
            }
            if epr.err_rx() {
                epr_value.set_err_rx(false);
                warn!("err_rx");
            }
            // Clear ctr (transaction complete) flags
            let rx_ready = epr.ctr_rx();
            let tx_ready = epr.ctr_tx();

            epr_value.set_ctr_rx(!rx_ready);
            epr_value.set_ctr_tx(!tx_ready);
            regs.epr(index).write_value(epr_value);

            if rx_ready {
                EP_IN_WAKERS[index].wake();
            }
            if tx_ready {
                EP_OUT_WAKERS[index].wake();
            }

            int_cleared = true;
        }

        if istr.err() {
            debug!("USB IRQ: err");
            regs.istr().write_value(regs::Istr(!0));

            // Write 0 to clear.
            let mut clear = regs::Istr(!0);
            clear.set_err(false);
            regs.istr().write_value(clear);

            let index = istr.ep_id() as usize;
            let mut epr = invariant(regs.epr(index).read());
            // Toggle endponit to disabled
            epr.set_stat_rx(epr.stat_rx());
            epr.set_stat_tx(epr.stat_tx());
            regs.epr(index).write_value(epr);

            int_cleared = true;
        }

        if int_cleared == false {
            // Write 0 to clear.
            let clear = regs::Istr(0);
            regs.istr().write_value(clear);
        }
    }
}

const EP_COUNT: usize = 8;

#[cfg(any(usbram_16x1_512, usbram_16x2_512))]
const USBRAM_SIZE: usize = 512;
#[cfg(any(usbram_16x2_1024, usbram_32_1024))]
const USBRAM_SIZE: usize = 1024;
#[cfg(usbram_32_2048)]
const USBRAM_SIZE: usize = 2048;

#[cfg(not(any(usbram_32_2048, usbram_32_1024)))]
const USBRAM_ALIGN: usize = 2;
#[cfg(any(usbram_32_2048, usbram_32_1024))]
const USBRAM_ALIGN: usize = 4;

const NEW_AW: AtomicWaker = AtomicWaker::new();
static BUS_WAKER: AtomicWaker = NEW_AW;
static EP_IN_WAKERS: [AtomicWaker; EP_COUNT] = [NEW_AW; EP_COUNT];
static EP_OUT_WAKERS: [AtomicWaker; EP_COUNT] = [NEW_AW; EP_COUNT];

fn convert_type(t: EndpointType) -> EpType {
    match t {
        EndpointType::Bulk => EpType::BULK,
        EndpointType::Control => EpType::CONTROL,
        EndpointType::Interrupt => EpType::INTERRUPT,
        EndpointType::Isochronous => EpType::ISO,
    }
}

fn invariant(mut r: regs::Epr) -> regs::Epr {
    r.set_ctr_rx(true); // don't clear
    r.set_ctr_tx(true); // don't clear
    r.set_dtog_rx(false); // don't toggle
    r.set_dtog_tx(false); // don't toggle
    r.set_stat_rx(Stat::from_bits(0));
    r.set_stat_tx(Stat::from_bits(0));
    r
}

fn align_len_up(len: u16) -> u16 {
    ((len as usize + USBRAM_ALIGN - 1) / USBRAM_ALIGN * USBRAM_ALIGN) as u16
}

/// Calculates the register field values for configuring receive buffer descriptor.
/// Returns `(actual_len, len_bits)`
///
/// `actual_len` length in bytes rounded up to USBRAM_ALIGN
/// `len_bits` should be placed on the upper 16 bits of the register value
fn calc_receive_len_bits(len: u16) -> (u16, u16) {
    match len {
        // NOTE: this could be 2..=62 with 16bit USBRAM, but not with 32bit. Limit it to 60 for simplicity.
        2..=60 => (align_len_up(len), align_len_up(len) / 2 << 10),
        61..=1024 => ((len + 31) / 32 * 32, (((len + 31) / 32 - 1) << 10) | 0x8000),
        _ => panic!("invalid OUT length {}", len),
    }
}

#[cfg(any(usbram_32_2048, usbram_32_1024))]
mod btable {
    use super::*;

    pub(super) fn write_in<T: Instance>(_index: usize, _addr: u16) {}

    // // Write to Transmit Buffer Descriptor for Channel/endpoint (USB_CHEP_TXRXBD_n)
    // // Device: IN endpoint
    // // Host: Out endpoint
    // // Address offset: n*8 [bytes] or n*2 in 32 bit words
    // pub(super) fn write_in_len<T: Instance>(index: usize, addr: u16, len: u16) {
    //     USBRAM.mem(index * 2).write_value((addr as u32) | ((len as u32) << 16));
    // }

    // TODO: Replaces write_in_len
    /// Writes to Transmit Buffer Descriptor for Channel/endpoint `index``
    /// For Device this is an IN endpoint for Host an OUT endpoint
    pub(super) fn write_transmit_buffer_descriptor<T: Instance>(index: usize, addr: u16, len: u16) {
        // Address offset: index*8 [bytes] thus index*2 in 32 bit words
        USBRAM
            .mem(index * 2)
            .write_value((addr as u32) | ((len as u32) << 16));
    }

    // Replaces write_out
    /// Writes to Receive Buffer Descriptor for Channel/endpoint `index``
    /// For Device this is an OUT endpoint for Host an IN endpoint
    pub(super) fn write_receive_buffer_descriptor<T: Instance>(
        index: usize,
        addr: u16,
        max_len_bits: u16,
    ) {
        // Address offset: index*8 + 4 [bytes] thus index*2 + 1 in 32 bit words
        USBRAM
            .mem(index * 2 + 1)
            .write_value((addr as u32) | ((max_len_bits as u32) << 16));
    }

    pub(super) fn read_out_len<T: Instance>(index: usize) -> u16 {
        (USBRAM.mem(index * 2 + 1).read() >> 16) as u16
    }
}

// Maybe replace with struct that only knows its index
struct EndpointBuffer<T: Instance> {
    addr: u16,
    len: u16,
    _phantom: PhantomData<T>,
}

impl<T: Instance> EndpointBuffer<T> {
    pub fn read(&mut self, buf: &mut [u8]) {
        assert!(buf.len() <= self.len as usize);
        for i in 0..(buf.len() + USBRAM_ALIGN - 1) / USBRAM_ALIGN {
            let val = USBRAM.mem(self.addr as usize / USBRAM_ALIGN + i).read();
            let n = USBRAM_ALIGN.min(buf.len() - i * USBRAM_ALIGN);
            buf[i * USBRAM_ALIGN..][..n].copy_from_slice(&val.to_le_bytes()[..n]);
        }
    }

    fn write(&mut self, buf: &[u8]) {
        assert!(buf.len() <= self.len as usize);
        for i in 0..(buf.len() + USBRAM_ALIGN - 1) / USBRAM_ALIGN {
            let mut val = [0u8; USBRAM_ALIGN];
            let n = USBRAM_ALIGN.min(buf.len() - i * USBRAM_ALIGN);
            val[..n].copy_from_slice(&buf[i * USBRAM_ALIGN..][..n]);

            #[cfg(not(any(usbram_32_2048, usbram_32_1024)))]
            let val = u16::from_le_bytes(val);
            #[cfg(any(usbram_32_2048, usbram_32_1024))]
            let val = u32::from_le_bytes(val);
            USBRAM
                .mem(self.addr as usize / USBRAM_ALIGN + i)
                .write_value(val);
        }
    }
}

/// USB host driver.
pub struct USBHostDriver<'d, T: Instance> {
    phantom: PhantomData<&'d mut T>,
    ep_mem_free: u16, // first free address in EP mem, in bytes.
    control_channel_in: Channel<'d, T, In>,
    control_channel_out: Channel<'d, T, Out>,
    channels_in_used: u8,
    channels_out_used: u8,
}

impl<'d, T: Instance> USBHostDriver<'d, T> {
    /// Create a new USB driver.
    pub fn new(
        _usb: impl Peripheral<P = T> + 'd,
        _irq: impl interrupt::typelevel::Binding<T::Interrupt, USBHostInterruptHandler<T>> + 'd,
        dp: impl Peripheral<P = impl DpPin<T>> + 'd,
        dm: impl Peripheral<P = impl DmPin<T>> + 'd,
    ) -> Self {
        into_ref!(dp, dm);

        super::super::common_init::<T>();

        let regs = T::regs();

        regs.cntr().write(|w| {
            w.set_pdwn(false);
            w.set_fres(true);
            w.set_host(true);
        });

        // Wait for voltage reference
        #[cfg(feature = "time")]
        embassy_time::block_for(embassy_time::Duration::from_millis(100));
        #[cfg(not(feature = "time"))]
        cortex_m::asm::delay(unsafe { crate::rcc::get_freqs() }.sys.unwrap().0 / 10);

        #[cfg(not(usb_v4))]
        regs.btable().write(|w| w.set_btable(0));

        // #[cfg(not(stm32l1))]
        // {
        //     use crate::gpio::{AfType, OutputType, Speed};
        //     dp.set_as_af(
        //         dp.af_num(),
        //         AfType::output(OutputType::PushPull, Speed::VeryHigh),
        //     );
        //     dm.set_as_af(
        //         dm.af_num(),
        //         AfType::output(OutputType::PushPull, Speed::VeryHigh),
        //     );
        // }

        // #[cfg(stm32l1)]
        let _ = (dp, dm); // suppress "unused" warnings.

        crate::pac::RCC.pllcfgr(1).modify(|w| w.set_pllqen(true));
        crate::pac::PWR.usbscr().modify(|w| w.set_usb33sv(true));
        crate::pac::RCC.apb2enr().modify(|w| w.set_usben(true));

        Self {
            phantom: PhantomData,
            ep_mem_free: EP_COUNT as u16 * 8, // for each EP, 4 regs, so 8 bytes
            control_channel_in: Channel::new(0, 0, 0, 0),
            control_channel_out: Channel::new(0, 0, 0, 0),
            channels_in_used: 0,
            channels_out_used: 0,
        }
    }

    /// Start the USB peripheral
    pub fn start(&mut self) {
        let _ = self.reconfigure_channel0(8, 0);

        let regs = T::regs();

        let _istr = regs.istr().read();

        regs.cntr().write(|w| {
            w.set_host(false);
        });

        regs.cntr().write(|w| {
            w.set_fres(true);
        });

        // Enable pull downs on DP and DM lines for host mode
        #[cfg(any(usb_v3, usb_v4))]
        regs.bcdr().write(|w| w.set_dppu(true));

        regs.cntr().write(|w| {
            // Masks
            w.set_suspm(true);
            w.set_wkupm(true);

            w.set_ctrm(true);
            w.set_resetm(true);
            w.set_errm(true);
            w.set_sofm(true);
            w.set_pmaovrm(true);
            w.set_esofm(true);
            w.set_l1reqm(true);
            w.set_host(true);
            w.set_pdwn(true);
            w.set_fres(true);
        });

        #[cfg(feature = "time")]
        embassy_time::block_for(embassy_time::Duration::from_millis(100));
        regs.cntr().write(|w| {
            // Masks
            w.set_suspm(true);
            w.set_wkupm(true);

            w.set_ctrm(true);
            w.set_resetm(true);
            w.set_errm(true);
            w.set_sofm(true);
            w.set_pmaovrm(true);
            w.set_esofm(true);
            w.set_l1reqm(true);
            w.set_host(true);
            w.set_pdwn(false);
            w.set_fres(true);
        });

        #[cfg(feature = "time")]
        embassy_time::block_for(embassy_time::Duration::from_millis(100));
        regs.cntr().write(|w| {
            // Masks
            w.set_suspm(true);
            w.set_wkupm(true);

            w.set_ctrm(true);
            w.set_resetm(true);
            w.set_errm(true);
            w.set_sofm(true);
            w.set_pmaovrm(true);
            w.set_esofm(true);
            w.set_l1reqm(true);
            w.set_host(true);
            w.set_pdwn(false);
            w.set_fres(false);
        });

        // regs.cntr().write(|w| {
        //     w.set_fres(true);
        // });

        // regs.cntr().write(|w| {
        //     w.set_pdwn(true);
        // });

        // regs.cntr().write(|w| {});

        #[cfg(stm32l1)]
        crate::pac::SYSCFG.pmc().modify(|w| w.set_usb_pu(true));

        // HostControlPipe::new(ep_in, ep_out, control_max_packet_size)
    }

    pub fn get_status(&self) -> u32 {
        let regs = T::regs();

        let istr = regs.istr().read();

        istr.0
    }

    fn reset_alloc(&mut self) {
        // Reset alloc pointer.
        self.ep_mem_free = EP_COUNT as u16 * 8; // for each EP, 4 regs, so 8 bytes

        self.channels_in_used = 0;
        self.channels_out_used = 0;
    }

    fn alloc_channel_mem(&mut self, len: u16) -> Result<u16, ()> {
        assert!(len as usize % USBRAM_ALIGN == 0);
        let addr = self.ep_mem_free;
        if addr + len > USBRAM_SIZE as _ {
            // panic!("Endpoint memory full");
            error!("Endpoint memory full");
            return Err(());
        }
        self.ep_mem_free += len;
        Ok(addr)
    }

    fn claim_channel_in(
        &mut self,
        index: usize,
        max_packet_size: u16,
        ep_type: EpType,
        dev_addr: u8,
    ) -> Result<Channel<'d, T, In>, ()> {
        if self.channels_in_used & (1 << index) != 0 {
            error!("Channel {} In already in use", index);
            return Err(());
        }

        self.channels_in_used |= 1 << index;

        let (len, len_bits) = calc_receive_len_bits(max_packet_size);
        let Ok(addr) = self.alloc_channel_mem(len) else {
            return Err(());
        };

        btable::write_receive_buffer_descriptor::<T>(index, addr, len_bits);

        let in_channel: Channel<T, In> = Channel::new(index, addr, len, max_packet_size);

        // configure channel register
        let epr_reg = T::regs().epr(index);
        let mut epr = invariant(epr_reg.read());
        epr.set_devaddr(dev_addr);
        epr.set_ep_type(ep_type);
        epr.set_ea(index as _);
        epr_reg.write_value(epr);

        Ok(in_channel)
    }

    fn claim_channel_out(
        &mut self,
        index: usize,
        max_packet_size: u16,
        ep_type: EpType,
        dev_addr: u8,
    ) -> Result<Channel<'d, T, Out>, ()> {
        if self.channels_out_used & (1 << index) != 0 {
            error!("Channel {} In already in use", index);
            return Err(());
        }
        self.channels_out_used |= 1 << index;

        let len = align_len_up(max_packet_size);
        let Ok(addr) = self.alloc_channel_mem(len) else {
            return Err(());
        };

        // ep_in_len is written when actually TXing packets.
        btable::write_in::<T>(index, addr);

        let out_channel: Channel<T, Out> = Channel::new(index, addr, len, max_packet_size);

        // configure channel register
        let epr_reg = T::regs().epr(index);
        let mut epr = invariant(epr_reg.read());
        epr.set_devaddr(dev_addr);
        epr.set_ep_type(ep_type);
        epr.set_ea(index as _);
        epr_reg.write_value(epr);

        Ok(out_channel)
    }
}

/// Marker type for the "IN" direction.
pub enum In {}

/// Marker type for the "OUT" direction.
pub enum Out {}

/// USB endpoint.
pub struct Channel<'d, T: Instance, D> {
    _phantom: PhantomData<(&'d mut T, D)>,
    index: usize,
    max_packet_size: u16,
    buf: EndpointBuffer<T>,
}

impl<'d, T: Instance, D> Channel<'d, T, D> {
    fn new(index: usize, addr: u16, len: u16, max_packet_size: u16) -> Self {
        Self {
            _phantom: PhantomData,
            index,
            max_packet_size,
            buf: EndpointBuffer {
                addr,
                len,
                _phantom: PhantomData,
            },
        }
    }

    fn reg(&self) -> Reg<Epr, RW> {
        T::regs().epr(self.index)
    }
}

impl<'d, T: Instance> Channel<'d, T, In> {
    fn read_data(&mut self, buf: &mut [u8]) -> Result<usize, ChannelError> {
        let index = self.index;
        let rx_len = btable::read_out_len::<T>(index) as usize & 0x3FF;
        trace!("READ DONE, rx_len = {}", rx_len);
        if rx_len > buf.len() {
            return Err(ChannelError::BufferOverflow);
        }
        self.buf.read(&mut buf[..rx_len]);
        Ok(rx_len)
    }

    pub fn activate(&mut self) {
        let epr = self.reg();
        let epr_val = epr.read();
        let current_stat_rx = epr_val.stat_rx().to_bits();
        let mut epr_val = invariant(epr_val);
        // stat_rx can only be toggled by writing a 1.
        // We want to set it to Valid (0b11)
        let stat_mask = Stat::from_bits(!current_stat_rx & 0x3);
        epr_val.set_stat_rx(stat_mask);
        epr.write_value(epr_val);
    }

    pub fn disable(&mut self) {
        let epr = self.reg();
        let epr_val = epr.read();
        let current_stat_rx = epr_val.stat_rx();
        let mut epr_val = invariant(epr_val);
        // stat_rx can only be toggled by writing a 1.
        // We want to set it to Disabled (0b00).
        epr_val.set_stat_rx(current_stat_rx);
        epr.write_value(epr_val);
    }
}

impl<'d, T: Instance> ChannelIn for Channel<'d, T, In> {
    async fn read(
        &mut self,
        buf: &mut [u8],
        options: impl Into<Option<TransferOptions>>,
    ) -> Result<usize, ChannelError> {
        let index = self.index;

        let options: TransferOptions = options.into().unwrap_or_default();

        let regs = T::regs();
        self.activate();

        let mut count: usize = 0;

        let t0 = Instant::now();

        poll_fn(|cx| {
            EP_IN_WAKERS[index].register(cx.waker());

            // Detect disconnect
            let istr = regs.istr().read();
            if !istr.dcon_stat() {
                self.disable();
                return Poll::Ready(Err(ChannelError::Disconnected));
            }

            if let Some(timeout_ms) = options.timeout_ms {
                if t0.elapsed() > Duration::from_millis(timeout_ms as u64) {
                    self.disable();
                    return Poll::Ready(Err(ChannelError::Timeout));
                }
            }

            let stat = self.reg().read().stat_rx();
            match stat {
                Stat::DISABLED => {
                    // Data available for read
                    let idest = &mut buf[count..];
                    let n = self.read_data(idest)?;
                    count += n;
                    // If transfer is smaller than max_packet_size, we are done
                    // If we have read buf.len() bytes, we are done
                    if count == buf.len() || n < self.max_packet_size as usize {
                        Poll::Ready(Ok(count))
                    } else {
                        // More data expected: issue another read.
                        self.activate();
                        Poll::Pending
                    }
                }
                Stat::STALL => {
                    // error
                    Poll::Ready(Err(ChannelError::Stall))
                }
                Stat::NAK => Poll::Pending,
                Stat::VALID => {
                    // not started yet? Try again
                    Poll::Pending
                }
            }
        })
        .await
    }
}

impl<'d, T: Instance> Channel<'d, T, Out> {
    fn write_data(&mut self, buf: &[u8]) {
        let index = self.index;
        self.buf.write(buf);
        btable::write_transmit_buffer_descriptor::<T>(index, self.buf.addr, buf.len() as _);
    }

    pub fn activate(&mut self) {
        let epr = self.reg();
        let epr_val = epr.read();
        let current_stat_tx = epr_val.stat_tx().to_bits();
        let mut epr_val = invariant(epr_val);
        // stat_tx can only be toggled by writing a 1.
        // We want to set it to Valid (0b11)
        let stat_mask = Stat::from_bits(!current_stat_tx & 0x3);
        epr_val.set_stat_tx(stat_mask);
        epr.write_value(epr_val);
    }

    fn disable(&mut self) {
        let epr = self.reg();
        let epr_val = epr.read();
        let current_stat_tx = epr_val.stat_tx();
        let mut epr_val = invariant(epr_val);
        // stat_tx can only be toggled by writing a 1.
        // We want to set it to InActive (0b00).
        epr_val.set_stat_tx(current_stat_tx);
        epr.write_value(epr_val);
    }
}

impl<'d, T: Instance> ChannelOut for Channel<'d, T, Out> {
    async fn write(
        &mut self,
        buf: &[u8],
        options: impl Into<Option<TransferOptions>>,
    ) -> Result<(), ChannelError> {
        self.write_data(buf);

        let index = self.index;

        let options: TransferOptions = options.into().unwrap_or_default();

        let regs = T::regs();

        self.activate();

        let t0 = Instant::now();

        poll_fn(|cx| {
            EP_OUT_WAKERS[index].register(cx.waker());

            // Detect disconnect
            let istr = regs.istr().read();
            if !istr.dcon_stat() {
                self.disable();
                return Poll::Ready(Err(ChannelError::Disconnected));
            }

            if let Some(timeout_ms) = options.timeout_ms {
                if t0.elapsed() > Duration::from_millis(timeout_ms as u64) {
                    // Timeout, we need to stop the current transaction.
                    self.disable();
                    return Poll::Ready(Err(ChannelError::Timeout));
                }
            }

            let stat = self.reg().read().stat_tx();
            match stat {
                Stat::DISABLED => Poll::Ready(Ok(())),
                Stat::STALL => Poll::Ready(Err(ChannelError::Stall)),
                Stat::NAK | Stat::VALID => Poll::Pending,
            }
        })
        .await
    }
}

impl<'d, T: Instance> USBHostDriverTrait for USBHostDriver<'d, T> {
    type ChannelIn = Channel<'d, T, In>;
    type ChannelOut = Channel<'d, T, Out>;

    fn alloc_channel_in(&mut self, desc: &EndpointDescriptor) -> Result<Self::ChannelIn, ()> {
        let index = (desc.endpoint_address - 0x80) as usize;

        if index == 0 {
            return Err(());
        }
        if index > EP_COUNT - 1 {
            return Err(());
        }
        let max_packet_size = desc.max_packet_size;
        let ep_type = desc.ep_type();
        debug!(
            "alloc_channel_in: index = {}, max_packet_size = {}, type = {:?}",
            index, max_packet_size, ep_type
        );

        // read current device address from channel 0
        let epr_reg = T::regs().epr(0);
        let addr = epr_reg.read().devaddr();

        self.claim_channel_in(index, max_packet_size, convert_type(ep_type), addr)
    }

    fn alloc_channel_out(&mut self, desc: &EndpointDescriptor) -> Result<Self::ChannelOut, ()> {
        let index = desc.endpoint_address as usize;
        if index == 0 {
            return Err(());
        }
        if index > EP_COUNT - 1 {
            return Err(());
        }
        let max_packet_size = desc.max_packet_size;
        let ep_type = desc.ep_type();

        // read current device address from channel 0
        let epr_reg = T::regs().epr(0);
        let addr = epr_reg.read().devaddr();

        self.claim_channel_out(index, max_packet_size, convert_type(ep_type), addr)
    }

    fn reconfigure_channel0(&mut self, max_packet_size: u16, dev_addr: u8) -> Result<(), ()> {
        // Clear all buffer memory
        self.reset_alloc();

        self.control_channel_in =
            self.claim_channel_in(0, max_packet_size, EpType::CONTROL, dev_addr)?;
        self.control_channel_out =
            self.claim_channel_out(0, max_packet_size, EpType::CONTROL, dev_addr)?;

        Ok(())
    }

    async fn bus_reset(&mut self) {
        let regs = T::regs();

        trace!("Bus reset");
        // Set bus in reset state
        regs.cntr().modify(|w| {
            w.set_fres(true);
        });

        // USB Spec says wait 50ms
        Timer::after_millis(50).await;

        // Clear reset state; device will be in default state
        regs.cntr().modify(|w| {
            w.set_fres(false);
        });
    }

    async fn wait_for_device_connect(&mut self) {
        poll_fn(|cx| {
            let istr = T::regs().istr().read();

            BUS_WAKER.register(cx.waker());

            if istr.dcon_stat() {
                // device has been detected
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    async fn wait_for_device_disconnect(&mut self) {
        poll_fn(|cx| {
            let istr = T::regs().istr().read();

            BUS_WAKER.register(cx.waker());

            if !istr.dcon_stat() {
                // device has dosconnected
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    async fn control_request_out(&mut self, bytes: &[u8], data: &[u8]) -> Result<(), ()> {
        let epr0 = T::regs().epr(0);

        // setup stage
        let mut epr_val = invariant(epr0.read());
        epr_val.set_setup(true);
        epr0.write_value(epr_val);
        let options = TransferOptions::default().set_timeout_ms(1000);
        self.control_channel_out
            .write(bytes, options.clone())
            .await
            .map_err(|_| ())?;

        // data stage
        if data.len() > 0 {
            self.control_channel_out
                .write(data, options.clone())
                .await
                .map_err(|_| ())?;
        }

        // Status stage
        let mut status = [0u8; 0];
        self.control_channel_in
            .read(&mut status, options)
            .await
            .map_err(|_| ())?;

        Ok(())
    }

    async fn control_request_in(&mut self, bytes: &[u8], dest: &mut [u8]) -> Result<usize, ()> {
        let epr0 = T::regs().epr(0);

        // setup stage
        let mut epr_val = invariant(epr0.read());
        epr_val.set_setup(true);
        epr0.write_value(epr_val);
        let options = TransferOptions::default().set_timeout_ms(50);

        self.control_channel_out
            .write(bytes, options.clone())
            .await
            .map_err(|_| ())?;

        // data stage
        let count = self
            .control_channel_in
            .read(dest, options.clone())
            .await
            .map_err(|_| ())?;

        // status stage

        // Send 0 bytes
        let zero = [0u8; 0];
        self.control_channel_out
            .write(&zero, options)
            .await
            .map_err(|_| ())?;

        Ok(count)
    }
}