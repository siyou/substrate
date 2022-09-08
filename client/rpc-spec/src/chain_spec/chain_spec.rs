// This file is part of Substrate.

// Copyright (C) 2020-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! API implementation for the specification of a chain.

use crate::chain_spec::api::ChainSpecApiServer;
use jsonrpsee::core::RpcResult;

/// An API for chain spec RPC calls.
pub struct ChainSpec {
	spec: Box<dyn sc_chain_spec::ChainSpec>,
}

impl ChainSpec {
	/// Create a new [`ChainSpec`].
	pub fn new(spec: Box<dyn sc_chain_spec::ChainSpec>) -> Self {
		Self { spec }
	}
}

impl ChainSpecApiServer for ChainSpec {
	fn chainspec_unstable_properties(&self) -> RpcResult<String> {
		let properties = self.spec.properties();

		// TODO: Propagate error.
		let ret = serde_json::to_string(&properties).unwrap();
		Ok(ret)
	}
}
