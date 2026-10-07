from typing import Literal

TransactionSubmissionObject = Literal["transaction_submission"]

TRANSACTION_SUBMISSION_OBJECT_VALUES: set[TransactionSubmissionObject] = {
    "transaction_submission",
}


def check_transaction_submission_object(value: str) -> TransactionSubmissionObject:
    if value in TRANSACTION_SUBMISSION_OBJECT_VALUES:
        return value
    raise TypeError(
        f"Unexpected value {value!r}. Expected one of {TRANSACTION_SUBMISSION_OBJECT_VALUES!r}"
    )
