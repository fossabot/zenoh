//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
use super::common::{conduit::TransportConduitTx, pipeline::TransmissionPipeline};
use super::protocol::core::Priority;
use super::protocol::io::{ZBuf, ZSlice};
use super::protocol::proto::TransportMessage;
use super::transport::TransportUnicastInner;
#[cfg(feature = "stats")]
use super::transport::TransportUnicastStatsInner;
use crate::net::link::LinkUnicast;
use async_std::prelude::*;
use async_std::task;
use async_std::task::JoinHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use zenoh_util::collections::RecyclingObjectPool;
use zenoh_util::core::{ZError, ZErrorKind, ZResult};
use zenoh_util::sync::Signal;
use zenoh_util::zerror;

#[derive(Clone)]
pub(super) struct TransportLinkUnicast {
    // The underlying link
    pub(super) inner: LinkUnicast,
    // The transport this link is associated to
    transport: TransportUnicastInner,
    // The transmission pipeline
    pipeline: Option<Arc<TransmissionPipeline>>,
    // The signals to stop TX/RX tasks
    handle_tx: Option<Arc<JoinHandle<()>>>,
    active_rx: Arc<AtomicBool>,
    signal_rx: Signal,
    handle_rx: Option<Arc<JoinHandle<()>>>,
}

impl TransportLinkUnicast {
    pub(super) fn new(transport: TransportUnicastInner, link: LinkUnicast) -> TransportLinkUnicast {
        TransportLinkUnicast {
            transport,
            inner: link,
            pipeline: None,
            handle_tx: None,
            active_rx: Arc::new(AtomicBool::new(false)),
            signal_rx: Signal::new(),
            handle_rx: None,
        }
    }
}

impl TransportLinkUnicast {
    #[inline]
    pub(super) fn get_link(&self) -> &LinkUnicast {
        &self.inner
    }

    #[inline]
    pub(super) fn get_pipeline(&self) -> Option<Arc<TransmissionPipeline>> {
        self.pipeline.clone()
    }

    pub(super) fn start_tx(
        &mut self,
        keep_alive: Duration,
        batch_size: u16,
        conduit_tx: Arc<[TransportConduitTx]>,
    ) {
        if self.handle_tx.is_none() {
            // The pipeline
            let pipeline = Arc::new(TransmissionPipeline::new(
                batch_size.min(self.inner.get_mtu()),
                self.inner.is_streamed(),
                conduit_tx,
            ));
            self.pipeline = Some(pipeline.clone());

            // Spawn the TX task
            let c_link = self.inner.clone();
            let c_transport = self.transport.clone();
            let handle = task::spawn(async move {
                let res = tx_task(
                    pipeline,
                    c_link.clone(),
                    keep_alive,
                    #[cfg(feature = "stats")]
                    c_transport.stats.clone(),
                )
                .await;
                if let Err(e) = res {
                    log::debug!("{}", e);
                    // Spawn a task to avoid a deadlock waiting for this same task
                    // to finish in the close() joining its handle
                    task::spawn(async move { c_transport.del_link(&c_link).await });
                }
            });
            self.handle_tx = Some(Arc::new(handle));
        }
    }

    pub(super) fn stop_tx(&mut self) {
        if let Some(pipeline) = self.pipeline.take() {
            pipeline.disable();
        }
    }

    pub(super) fn start_rx(&mut self, lease: Duration) {
        if self.handle_rx.is_none() {
            self.active_rx.store(true, Ordering::Release);
            // Spawn the RX task
            let c_link = self.inner.clone();
            let c_transport = self.transport.clone();
            let c_signal = self.signal_rx.clone();
            let c_active = self.active_rx.clone();
            let c_rx_buff_size = self.transport.manager.config.link_rx_buff_size;

            let handle = task::spawn(async move {
                // Start the consume task
                let res = rx_task(
                    c_link.clone(),
                    c_transport.clone(),
                    lease,
                    c_signal.clone(),
                    c_active.clone(),
                    c_rx_buff_size,
                )
                .await;
                c_active.store(false, Ordering::Release);
                if let Err(e) = res {
                    log::debug!("{}", e);
                    // Spawn a task to avoid a deadlock waiting for this same task
                    // to finish in the close() joining its handle
                    task::spawn(async move { c_transport.del_link(&c_link).await });
                }
            });
            self.handle_rx = Some(Arc::new(handle));
        }
    }

    pub(super) fn stop_rx(&mut self) {
        self.active_rx.store(false, Ordering::Release);
        self.signal_rx.trigger();
    }

    pub(super) async fn close(mut self) -> ZResult<()> {
        log::trace!("{}: closing", self.inner);
        self.stop_rx();
        if let Some(handle) = self.handle_rx.take() {
            // It is safe to unwrap the Arc since we have the ownership of the whole link
            let handle_rx = Arc::try_unwrap(handle).unwrap();
            handle_rx.await;
        }

        self.stop_tx();
        if let Some(handle) = self.handle_tx.take() {
            // It is safe to unwrap the Arc since we have the ownership of the whole link
            let handle_tx = Arc::try_unwrap(handle).unwrap();
            handle_tx.await;
        }

        self.inner.close().await
    }
}

/*************************************/
/*              TASKS                */
/*************************************/
async fn tx_task(
    pipeline: Arc<TransmissionPipeline>,
    link: LinkUnicast,
    keep_alive: Duration,
    #[cfg(feature = "stats")] stats: TransportUnicastStatsInner,
) -> ZResult<()> {
    loop {
        match pipeline.pull().timeout(keep_alive).await {
            Ok(res) => match res {
                Some((batch, priority)) => {
                    // Send the buffer on the link
                    let bytes = batch.as_bytes();
                    let _ = link.write_all(bytes).await?;

                    #[cfg(feature = "stats")]
                    {
                        stats.inc_tx_msgs(batch.stats.t_msgs);
                        stats.inc_tx_bytes(bytes.len());
                    }

                    // Reinsert the batch into the queue
                    pipeline.refill(batch, priority);
                }
                None => break,
            },
            Err(_) => {
                let pid = None;
                let attachment = None;
                let message = TransportMessage::make_keep_alive(pid, attachment);
                pipeline.push_transport_message(message, Priority::Background);
            }
        }
    }

    // Drain the transmission pipeline and write remaining bytes on the wire
    let mut batches = pipeline.drain();
    for (b, _) in batches.drain(..) {
        let _ = link
            .write_all(b.as_bytes())
            .timeout(keep_alive)
            .await
            .map_err(|_| {
                let e = format!("{}: flush failed after {} ms", link, keep_alive.as_millis());
                zerror2!(ZErrorKind::IoError { descr: e })
            })??;

        #[cfg(feature = "stats")]
        {
            stats.inc_tx_msgs(b.stats.t_msgs);
            stats.inc_tx_bytes(b.len());
        }
    }

    Ok(())
}

async fn rx_task_stream(
    link: LinkUnicast,
    transport: TransportUnicastInner,
    lease: Duration,
    signal: Signal,
    active: Arc<AtomicBool>,
    rx_buff_size: usize,
) -> ZResult<()> {
    enum Action {
        Read(usize),
        Stop,
    }

    async fn read(link: &LinkUnicast, buffer: &mut [u8]) -> ZResult<Action> {
        // 16 bits for reading the batch length
        let mut length = [0u8, 0u8];
        link.read_exact(&mut length).await?;
        let n = u16::from_le_bytes(length) as usize;
        link.read_exact(&mut buffer[0..n]).await?;
        Ok(Action::Read(n))
    }

    async fn stop(signal: Signal) -> ZResult<Action> {
        signal.wait().await;
        Ok(Action::Stop)
    }

    // The ZBuf to read a message batch onto
    let mut zbuf = ZBuf::new();
    // The pool of buffers
    let mtu = link.get_mtu() as usize;
    let n = 1 + (rx_buff_size / mtu);
    let pool = RecyclingObjectPool::new(n, || vec![0u8; mtu].into_boxed_slice());
    while active.load(Ordering::Acquire) {
        // Clear the ZBuf
        zbuf.clear();

        // Retrieve one buffer
        let mut buffer = pool.try_take().unwrap_or_else(|| pool.alloc());

        // Async read from the underlying link
        let action = read(&link, &mut buffer)
            .race(stop(signal.clone()))
            .timeout(lease)
            .await
            .map_err(|_| {
                let e = format!("{}: expired after {} milliseconds", link, lease.as_millis());
                zerror2!(ZErrorKind::IoError { descr: e })
            })??;
        match action {
            Action::Read(n) => {
                zbuf.add_zslice(ZSlice::new(buffer.into(), 0, n));

                #[cfg(feature = "stats")]
                transport.stats.inc_rx_bytes(2 + n); // Account for the batch len encoding (16 bits)

                while zbuf.can_read() {
                    match zbuf.read_transport_message() {
                        Some(msg) => {
                            #[cfg(feature = "stats")]
                            transport.stats.inc_rx_msgs(1);

                            transport.receive_message(msg, &link)?
                        }
                        None => {
                            let e = format!("{}: decoding error", link);
                            return zerror!(ZErrorKind::IoError { descr: e });
                        }
                    }
                }
            }
            Action::Stop => break,
        }
    }
    Ok(())
}

async fn rx_task_dgram(
    link: LinkUnicast,
    transport: TransportUnicastInner,
    lease: Duration,
    signal: Signal,
    active: Arc<AtomicBool>,
    rx_buff_size: usize,
) -> ZResult<()> {
    enum Action {
        Read(usize),
        Stop,
    }

    async fn read(link: &LinkUnicast, buffer: &mut [u8]) -> ZResult<Action> {
        let n = link.read(buffer).await?;
        Ok(Action::Read(n))
    }

    async fn stop(signal: Signal) -> ZResult<Action> {
        signal.wait().await;
        Ok(Action::Stop)
    }

    // The ZBuf to read a message batch onto
    let mut zbuf = ZBuf::new();
    // The pool of buffers
    let mtu = link.get_mtu() as usize;
    let n = 1 + (rx_buff_size / mtu);
    let pool = RecyclingObjectPool::new(n, || vec![0u8; mtu].into_boxed_slice());
    while active.load(Ordering::Acquire) {
        // Clear the zbuf
        zbuf.clear();
        // Retrieve one buffer
        let mut buffer = pool.try_take().unwrap_or_else(|| pool.alloc());

        // Async read from the underlying link
        let action = read(&link, &mut buffer)
            .race(stop(signal.clone()))
            .timeout(lease)
            .await
            .map_err(|_| {
                let e = format!("{}: expired after {} milliseconds", link, lease.as_millis());
                zerror2!(ZErrorKind::IoError { descr: e })
            })??;
        match action {
            Action::Read(n) => {
                if n == 0 {
                    // Reading 0 bytes means error
                    let e = format!("{}: zero bytes reading", link);
                    return zerror!(ZErrorKind::IoError { descr: e });
                }

                #[cfg(feature = "stats")]
                transport.stats.inc_rx_bytes(n);

                // Add the received bytes to the ZBuf for deserialization
                zbuf.add_zslice(ZSlice::new(buffer.into(), 0, n));

                // Deserialize all the messages from the current ZBuf
                while zbuf.can_read() {
                    match zbuf.read_transport_message() {
                        Some(msg) => {
                            #[cfg(feature = "stats")]
                            transport.stats.inc_rx_msgs(1);

                            transport.receive_message(msg, &link)?
                        }
                        None => {
                            let e = format!("{}: decoding error", link);
                            return zerror!(ZErrorKind::IoError { descr: e });
                        }
                    }
                }
            }
            Action::Stop => break,
        }
    }
    Ok(())
}

async fn rx_task(
    link: LinkUnicast,
    transport: TransportUnicastInner,
    lease: Duration,
    signal: Signal,
    active: Arc<AtomicBool>,
    rx_buff_size: usize,
) -> ZResult<()> {
    if link.is_streamed() {
        rx_task_stream(link, transport, lease, signal, active, rx_buff_size).await
    } else {
        rx_task_dgram(link, transport, lease, signal, active, rx_buff_size).await
    }
}
