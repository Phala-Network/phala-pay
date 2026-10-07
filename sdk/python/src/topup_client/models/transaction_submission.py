from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.transaction_submission_object import check_transaction_submission_object
from ..models.transaction_submission_object import TransactionSubmissionObject
from ..models.transaction_submission_status import check_transaction_submission_status
from ..models.transaction_submission_status import TransactionSubmissionStatus
from typing import cast


T = TypeVar("T", bound="TransactionSubmission")


@_attrs_define
class TransactionSubmission:
    """A quiet acknowledgement, including for ignored hints.

    Attributes:
        object_ (TransactionSubmissionObject): Constant submission object discriminator.
        status (TransactionSubmissionStatus): Received acknowledges submission only, never detection or verification.
        transaction_hash (str): Hash echoed from the submission.
    """

    object_: TransactionSubmissionObject
    status: TransactionSubmissionStatus
    transaction_hash: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        object_: str = self.object_

        status: str = self.status

        transaction_hash = self.transaction_hash

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "object": object_,
                "status": status,
                "transaction_hash": transaction_hash,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        object_ = check_transaction_submission_object(d.pop("object"))

        status = check_transaction_submission_status(d.pop("status"))

        transaction_hash = d.pop("transaction_hash")

        transaction_submission = cls(
            object_=object_,
            status=status,
            transaction_hash=transaction_hash,
        )

        transaction_submission.additional_properties = d
        return transaction_submission

    @property
    def additional_keys(self) -> list[str]:
        return list(self.additional_properties.keys())

    def __getitem__(self, key: str) -> Any:
        return self.additional_properties[key]

    def __setitem__(self, key: str, value: Any) -> None:
        self.additional_properties[key] = value

    def __delitem__(self, key: str) -> None:
        del self.additional_properties[key]

    def __contains__(self, key: str) -> bool:
        return key in self.additional_properties
