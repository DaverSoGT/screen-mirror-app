#[cfg(any(test, feature = "test-support"))]
pub mod qsv_ledger;

#[cfg(not(any(test, feature = "test-support")))]
#[expect(
    dead_code,
    reason = "Slice 1 ledger APIs remain unintegrated until Slice 2"
)]
pub(crate) mod qsv_ledger;
