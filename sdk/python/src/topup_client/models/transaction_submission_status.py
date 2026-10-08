from typing import Literal

TransactionSubmissionStatus = Literal["received"]

TRANSACTION_SUBMISSION_STATUS_VALUES: set[TransactionSubmissionStatus] = {
    "received",
}


def check_transaction_submission_status(value: str) -> TransactionSubmissionStatus:
    if value in TRANSACTION_SUBMISSION_STATUS_VALUES:
        return value
    raise TypeError(
        f"Unexpected value {value!r}. Expected one of {TRANSACTION_SUBMISSION_STATUS_VALUES!r}"
    )
