// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

#![allow(unused_imports)]
#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![cfg_attr(test, allow(unused_variables, unused_assignments))]

extern crate rand;
extern crate rand_chacha;
extern crate rusqlite;
extern crate serde;
#[macro_use]
extern crate lazy_static;
extern crate integer_sqrt;
extern crate regex;
extern crate time;

#[macro_use(o, slog_log, slog_trace, slog_debug, slog_info, slog_warn, slog_error)]
extern crate slog;

#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;

#[cfg(test)]
#[macro_use]
extern crate assert_json_diff;

#[cfg(any(test, feature = "testing"))]
#[macro_use]
extern crate rstest;

#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub extern crate rstest_reuse;

#[cfg(feature = "monitoring_prom")]
#[macro_use]
pub extern crate prometheus;

#[macro_use]
pub extern crate stacks_common;
pub use stacks_common::{
    impl_array_hexstring_fmt, impl_array_newtype, impl_byte_array_message_codec,
    impl_byte_array_serde,
};

pub use stacks_common::codec;
pub use stacks_common::consts;
pub use stacks_common::types;
pub use stacks_common::util;

#[macro_use]
/// The Clarity virtual machine
pub mod vm;

pub use stacks_common::address;

/// A high level library for interacting with the Clarity vm
// pub mod core;

pub mod boot_util {
    use crate::vm::representations::ContractName;
    use crate::vm::types::QualifiedContractIdentifier;
    use stacks_common::types::chainstate::StacksAddress;
    use std::convert::TryFrom;

    pub fn boot_code_id(name: &str, mainnet: bool) -> QualifiedContractIdentifier {
        let addr = boot_code_addr(mainnet);
        QualifiedContractIdentifier::new(
            addr.into(),
            ContractName::try_from(name.to_string()).unwrap(),
        )
    }

    pub fn boot_code_addr(mainnet: bool) -> StacksAddress {
        StacksAddress::burn_address(mainnet)
    }
}

// set via _compile-time_ envars
const GIT_BRANCH: Option<&'static str> = option_env!("GIT_BRANCH");
const GIT_COMMIT: Option<&'static str> = option_env!("GIT_COMMIT");
const GIT_TREE_CLEAN: Option<&'static str> = option_env!("GIT_TREE_CLEAN");

#[cfg(debug_assertions)]
const BUILD_TYPE: &str = "debug";
#[cfg(not(debug_assertions))]
const BUILD_TYPE: &str = "release";

pub fn version_string(pkg_name: &str, pkg_version: &str) -> String {
    let git_branch = GIT_BRANCH.unwrap_or("");
    let git_commit = GIT_COMMIT.unwrap_or("");
    let git_tree_clean = GIT_TREE_CLEAN.unwrap_or("");

    format!(
        "{} {} ({}:{}{}, {} build, {} [{}])",
        pkg_name,
        pkg_version,
        &git_branch,
        git_commit,
        git_tree_clean,
        BUILD_TYPE,
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}
