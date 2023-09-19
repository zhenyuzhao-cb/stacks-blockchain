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

use std::collections::HashMap;

use stacks_common::types::StacksEpochId;

use crate::vm::analysis::errors::{CheckError, CheckErrors, CheckResult};
use crate::vm::analysis::types::{AnalysisPass, ContractAnalysis};
use crate::vm::analysis::AnalysisDatabase;
use crate::vm::functions::define::{DefineFunctions, DefineFunctionsParsed};
use crate::vm::functions::NativeFunctions;
use crate::vm::representations::SymbolicExpressionType::{Atom, AtomValue, List, LiteralValue};
use crate::vm::representations::{ClarityName, SymbolicExpression};
use crate::vm::types::{FunctionType, TraitIdentifier, TypeSignature, Value};

pub struct TraitChecker {
    epoch: StacksEpochId,
}

impl AnalysisPass for TraitChecker {
    fn run_pass(
        epoch: &StacksEpochId,
        contract_analysis: &mut ContractAnalysis,
        analysis_db: &mut AnalysisDatabase,
    ) -> CheckResult<()> {
        let mut command = TraitChecker::new(epoch);
        command.run(contract_analysis, analysis_db)?;
        Ok(())
    }
}

impl TraitChecker {
    fn new(epoch: &StacksEpochId) -> Self {
        Self { epoch: *epoch }
    }

    pub fn run(
        &mut self,
        contract_analysis: &mut ContractAnalysis,
        analysis_db: &mut AnalysisDatabase,
    ) -> CheckResult<()> {
        for trait_identifier in &contract_analysis.implemented_traits {
            let trait_name = trait_identifier.name.to_string();
            let contract_defining_trait = analysis_db
                .load_contract(&trait_identifier.contract_identifier, &self.epoch)
                .ok_or(CheckErrors::TraitReferenceUnknown(
                    trait_identifier.name.to_string(),
                ))?;

            let trait_definition = contract_defining_trait
                .get_defined_trait(&trait_name)
                .ok_or(CheckErrors::TraitReferenceUnknown(
                    trait_identifier.name.to_string(),
                ))?;

            contract_analysis.check_trait_compliance(
                &self.epoch,
                trait_identifier,
                trait_definition,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
