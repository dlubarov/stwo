use itertools::{chain, Itertools};
use num_traits::One;
use tracing::{span, Level};

use crate::constraint_framework::constant_columns::gen_is_first;
use crate::constraint_framework::logup::{LogupAtRow, LogupTraceGenerator, LookupElements};
use crate::constraint_framework::{assert_constraints, EvalAtRow, FrameworkComponent};
use crate::core::air::{Air, AirProver, Component, ComponentProver};
use crate::core::backend::simd::column::BaseColumn;
use crate::core::backend::simd::m31::LOG_N_LANES;
use crate::core::backend::simd::qm31::PackedSecureField;
use crate::core::backend::simd::SimdBackend;
use crate::core::backend::Column;
use crate::core::channel::{Blake2sChannel, Channel};
use crate::core::fields::m31::BaseField;
use crate::core::fields::qm31::SecureField;
use crate::core::fields::IntoSlice;
use crate::core::pcs::CommitmentSchemeProver;
use crate::core::poly::circle::{CanonicCoset, CircleEvaluation, PolyOps};
use crate::core::poly::BitReversedOrder;
use crate::core::prover::{prove, StarkProof, LOG_BLOWUP_FACTOR};
use crate::core::vcs::blake2_hash::Blake2sHasher;
use crate::core::vcs::blake2_merkle::Blake2sMerkleHasher;
use crate::core::{ColumnVec, InteractionElements};

#[derive(Clone)]
pub struct PlonkAir {
    pub component: PlonkComponent,
}
impl Air for PlonkAir {
    fn components(&self) -> Vec<&dyn Component> {
        vec![&self.component]
    }
}
impl AirProver<SimdBackend> for PlonkAir {
    fn prover_components(&self) -> Vec<&dyn ComponentProver<SimdBackend>> {
        vec![&self.component]
    }
}

#[derive(Clone)]
pub struct PlonkComponent {
    pub log_n_rows: u32,
    pub lookup_elements: LookupElements<2>,
    pub claimed_sum: SecureField,
}

impl FrameworkComponent for PlonkComponent {
    fn log_size(&self) -> u32 {
        self.log_n_rows
    }

    fn max_constraint_log_degree_bound(&self) -> u32 {
        self.log_n_rows + 1
    }

    fn evaluate<E: EvalAtRow>(&self, mut eval: E) -> E {
        let [is_first] = eval.next_interaction_mask(2, [0]);
        let mut logup = LogupAtRow::<2, _>::new(1, self.claimed_sum, is_first);

        let [a_wire] = eval.next_interaction_mask(2, [0]);
        let [b_wire] = eval.next_interaction_mask(2, [0]);
        // Note: c_wire could also be implicit: (self.eval.point() - M31_CIRCLE_GEN.into_ef()).x.
        //   A constant column is easier though.
        let [c_wire] = eval.next_interaction_mask(2, [0]);
        let [op] = eval.next_interaction_mask(2, [0]);

        let mult = eval.next_trace_mask();
        let a_val = eval.next_trace_mask();
        let b_val = eval.next_trace_mask();
        let c_val = eval.next_trace_mask();

        eval.add_constraint(c_val - op * (a_val + b_val) + (E::F::one() - op) * a_val * b_val);

        logup.push_lookup(
            &mut eval,
            E::EF::one(),
            &[a_wire, a_val],
            &self.lookup_elements,
        );
        logup.push_lookup(
            &mut eval,
            E::EF::one(),
            &[b_wire, b_val],
            &self.lookup_elements,
        );
        logup.push_lookup(
            &mut eval,
            E::EF::from(-mult),
            &[c_wire, c_val],
            &self.lookup_elements,
        );

        logup.finalize(&mut eval);
        eval
    }
}

#[derive(Clone)]
pub struct PlonkCircuitTrace {
    pub mult: BaseColumn,
    pub a_wire: BaseColumn,
    pub b_wire: BaseColumn,
    pub c_wire: BaseColumn,
    pub op: BaseColumn,
    pub a_val: BaseColumn,
    pub b_val: BaseColumn,
    pub c_val: BaseColumn,
}
pub fn gen_trace(
    log_size: u32,
    circuit: &PlonkCircuitTrace,
) -> ColumnVec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>> {
    let _span = span!(Level::INFO, "Generation").entered();

    let domain = CanonicCoset::new(log_size).circle_domain();
    [
        &circuit.mult,
        &circuit.a_val,
        &circuit.b_val,
        &circuit.c_val,
    ]
    .into_iter()
    .map(|eval| CircleEvaluation::<SimdBackend, _, BitReversedOrder>::new(domain, eval.clone()))
    .collect_vec()
}

pub fn gen_interaction_trace(
    log_size: u32,
    circuit: &PlonkCircuitTrace,
    lookup_elements: &LookupElements<2>,
) -> (
    ColumnVec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>>,
    SecureField,
) {
    let _span = span!(Level::INFO, "Generate interaction trace").entered();
    let mut logup_gen = LogupTraceGenerator::new(log_size);

    let mut col_gen = logup_gen.new_col();
    for vec_row in 0..(1 << (log_size - LOG_N_LANES)) {
        let q0: PackedSecureField =
            lookup_elements.combine(&[circuit.a_wire.data[vec_row], circuit.a_val.data[vec_row]]);
        let q1: PackedSecureField =
            lookup_elements.combine(&[circuit.b_wire.data[vec_row], circuit.b_val.data[vec_row]]);
        col_gen.write_frac(vec_row, q0 + q1, q0 * q1);
    }
    col_gen.finalize_col();

    let mut col_gen = logup_gen.new_col();
    for vec_row in 0..(1 << (log_size - LOG_N_LANES)) {
        let p = -circuit.mult.data[vec_row];
        let q: PackedSecureField =
            lookup_elements.combine(&[circuit.c_wire.data[vec_row], circuit.c_val.data[vec_row]]);
        col_gen.write_frac(vec_row, p.into(), q);
    }
    col_gen.finalize_col();

    logup_gen.finalize()
}

#[allow(unused)]
pub fn prove_fibonacci_plonk(log_n_rows: u32) -> (PlonkAir, StarkProof<Blake2sMerkleHasher>) {
    assert!(log_n_rows >= LOG_N_LANES);

    // Prepare a fibonacci circuit.
    let mut fib_values = vec![BaseField::one(), BaseField::one()];
    for _ in 0..(1 << log_n_rows) {
        fib_values.push(fib_values[fib_values.len() - 1] + fib_values[fib_values.len() - 2]);
    }
    let range = 0..(1 << log_n_rows);
    let mut circuit = PlonkCircuitTrace {
        mult: range.clone().map(|_| 2.into()).collect(),
        a_wire: range.clone().map(|i| i.into()).collect(),
        b_wire: range.clone().map(|i| (i + 1).into()).collect(),
        c_wire: range.clone().map(|i| (i + 2).into()).collect(),
        op: range.clone().map(|_| 1.into()).collect(),
        a_val: range.clone().map(|i| fib_values[i]).collect(),
        b_val: range.clone().map(|i| fib_values[i + 1]).collect(),
        c_val: range.clone().map(|i| fib_values[i + 2]).collect(),
    };
    circuit.mult.set((1 << log_n_rows) - 1, 0.into());
    circuit.mult.set((1 << log_n_rows) - 2, 1.into());

    // Precompute twiddles.
    let span = span!(Level::INFO, "Precompute twiddles").entered();
    let twiddles = SimdBackend::precompute_twiddles(
        CanonicCoset::new(log_n_rows + LOG_BLOWUP_FACTOR + 1)
            .circle_domain()
            .half_coset,
    );
    span.exit();

    // Setup protocol.
    let channel = &mut Blake2sChannel::new(Blake2sHasher::hash(BaseField::into_slice(&[])));
    let commitment_scheme = &mut CommitmentSchemeProver::new(LOG_BLOWUP_FACTOR, &twiddles);

    // Trace.
    let span = span!(Level::INFO, "Trace").entered();
    let trace = gen_trace(log_n_rows, &circuit);
    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(trace);
    tree_builder.commit(channel);
    span.exit();

    // Draw lookup element.
    let lookup_elements = LookupElements::draw(channel);

    // Interaction trace.
    let span = span!(Level::INFO, "Interaction").entered();
    let (trace, claimed_sum) = gen_interaction_trace(log_n_rows, &circuit, &lookup_elements);
    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(trace);
    tree_builder.commit(channel);
    span.exit();

    // Constant trace.
    let span = span!(Level::INFO, "Constant").entered();
    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(
        chain!(
            [gen_is_first(log_n_rows)],
            [circuit.a_wire, circuit.b_wire, circuit.c_wire, circuit.op]
                .into_iter()
                .map(|col| {
                    CircleEvaluation::<SimdBackend, _, BitReversedOrder>::new(
                        CanonicCoset::new(log_n_rows).circle_domain(),
                        col,
                    )
                })
        )
        .collect_vec(),
    );
    tree_builder.commit(channel);
    span.exit();

    // Prove constraints.
    let component = PlonkComponent {
        log_n_rows,
        lookup_elements,
        claimed_sum,
    };

    // Sanity check. Remove for production.
    let trace_polys = commitment_scheme
        .trees
        .as_ref()
        .map(|t| t.polynomials.iter().cloned().collect_vec());
    assert_constraints(&trace_polys, CanonicCoset::new(log_n_rows), |mut eval| {
        component.evaluate(eval);
    });

    let air = PlonkAir { component };
    let proof = prove::<SimdBackend, _, _>(
        &air,
        channel,
        &InteractionElements::default(),
        commitment_scheme,
    )
    .unwrap();

    (air, proof)
}

#[cfg(test)]
mod tests {
    use std::env;

    use crate::constraint_framework::logup::LookupElements;
    use crate::core::air::AirExt;
    use crate::core::channel::{Blake2sChannel, Channel};
    use crate::core::fields::m31::BaseField;
    use crate::core::fields::IntoSlice;
    use crate::core::pcs::CommitmentSchemeVerifier;
    use crate::core::prover::verify;
    use crate::core::vcs::blake2_hash::Blake2sHasher;
    use crate::core::InteractionElements;
    use crate::examples::plonk::prove_fibonacci_plonk;

    #[test_log::test]
    fn test_simd_plonk_prove() {
        // Get from environment variable:
        let log_n_instances = env::var("LOG_N_INSTANCES")
            .unwrap_or_else(|_| "10".to_string())
            .parse::<u32>()
            .unwrap();

        // Prove.
        let (air, proof) = prove_fibonacci_plonk(log_n_instances);

        // Verify.
        // TODO: Create Air instance independently.
        let channel = &mut Blake2sChannel::new(Blake2sHasher::hash(BaseField::into_slice(&[])));
        let commitment_scheme = &mut CommitmentSchemeVerifier::new();

        // Decommit.
        // Retrieve the expected column sizes in each commitment interaction, from the AIR.
        let sizes = air.column_log_sizes();
        // Trace columns.
        commitment_scheme.commit(proof.commitments[0], &sizes[0], channel);
        // Draw lookup element.
        let lookup_elements = LookupElements::draw(channel);
        assert_eq!(lookup_elements, air.component.lookup_elements);
        // TODO(spapini): Check claimed sum against first and last instances.
        // Interaction columns.
        commitment_scheme.commit(proof.commitments[1], &sizes[1], channel);
        // Constant columns.
        commitment_scheme.commit(proof.commitments[2], &sizes[2], channel);

        verify(
            &air,
            channel,
            &InteractionElements::default(),
            commitment_scheme,
            proof,
        )
        .unwrap();
    }
}