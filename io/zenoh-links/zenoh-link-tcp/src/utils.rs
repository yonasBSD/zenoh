//
// Copyright (c) 2024 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use zenoh_config::Config as ZenohConfig;
use zenoh_link_commons::{
    tcp::TcpSocketConfig, ConfigurationInspector, BIND_INTERFACE, TCP_RX_BUFFER_SIZE,
    TCP_TX_BUFFER_SIZE,
};
use zenoh_protocol::core::{parameters, Config};
use zenoh_result::{zerror, ZResult};

#[derive(Default, Clone, Copy, Debug)]
pub struct TcpConfigurator;

impl ConfigurationInspector<ZenohConfig> for TcpConfigurator {
    fn inspect_config(&self, config: &ZenohConfig) -> ZResult<String> {
        let mut ps: Vec<(&str, &str)> = vec![];
        let c = config.transport().link();

        let rx_buffer_size;
        if let Some(size) = c.tcp_rx_buffer {
            rx_buffer_size = size.to_string();
            ps.push((TCP_RX_BUFFER_SIZE, &rx_buffer_size));
        }
        let tx_buffer_size;
        if let Some(size) = c.tcp_tx_buffer {
            tx_buffer_size = size.to_string();
            ps.push((TCP_TX_BUFFER_SIZE, &tx_buffer_size));
        }

        Ok(parameters::from_iter(ps.drain(..)))
    }
}

pub(crate) struct TcpLinkConfig<'a> {
    pub(crate) rx_buffer_size: Option<u32>,
    pub(crate) tx_buffer_size: Option<u32>,
    pub(crate) bind_iface: Option<&'a str>,
}

impl<'a> TcpLinkConfig<'a> {
    pub(crate) fn new(config: &'a Config) -> ZResult<Self> {
        let mut tcp_config = Self {
            rx_buffer_size: None,
            tx_buffer_size: None,
            bind_iface: config.get(BIND_INTERFACE),
        };

        if let Some(size) = config.get(TCP_RX_BUFFER_SIZE) {
            tcp_config.rx_buffer_size = Some(
                size.parse()
                    .map_err(|_| zerror!("Unknown TCP read buffer size argument: {}", size))?,
            );
        };
        if let Some(size) = config.get(TCP_TX_BUFFER_SIZE) {
            tcp_config.tx_buffer_size = Some(
                size.parse()
                    .map_err(|_| zerror!("Unknown TCP write buffer size argument: {}", size))?,
            );
        };

        Ok(tcp_config)
    }
}

impl<'a> From<TcpLinkConfig<'a>> for TcpSocketConfig<'a> {
    fn from(value: TcpLinkConfig<'a>) -> Self {
        Self::new(value.tx_buffer_size, value.rx_buffer_size, value.bind_iface)
    }
}
