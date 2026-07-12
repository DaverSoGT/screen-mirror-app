#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Slice 1 ledger APIs remain unintegrated until Slice 2"
    )
)]
pub(crate) mod qsv_ledger;
