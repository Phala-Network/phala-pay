from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.error_type import check_error_type
from ..models.error_type import ErrorType
from ..types import UNSET, Unset
from typing import cast


T = TypeVar("T", bound="ErrorDetail")


@_attrs_define
class ErrorDetail:
    """Stable error fields safe to expose to callers.

    Attributes:
        code (str): Stable machine-readable error code; clients accept future codes.
        message (str): Human-readable summary without internal details; it may change.
        type_ (ErrorType): Error category of [`ErrorDetail`].
        doc_url (str | Unset): The documentation of `code` in the API reference. Every error of this service carries it;
            it is optional in the schema, as in Stripe's, so a client never fails on an error without
            it.
        param (None | str | Unset): The request parameter the error is about, when there is one.
    """

    code: str
    message: str
    type_: ErrorType
    doc_url: str | Unset = UNSET
    param: None | str | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        code = self.code

        message = self.message

        type_: str = self.type_

        doc_url = self.doc_url

        param: None | str | Unset
        if isinstance(self.param, Unset):
            param = UNSET
        else:
            param = self.param

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "code": code,
                "message": message,
                "type": type_,
            }
        )
        if doc_url is not UNSET:
            field_dict["doc_url"] = doc_url
        if param is not UNSET:
            field_dict["param"] = param

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        code = d.pop("code")

        message = d.pop("message")

        type_ = check_error_type(d.pop("type"))

        doc_url = d.pop("doc_url", UNSET)

        def _parse_param(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        param = _parse_param(d.pop("param", UNSET))

        error_detail = cls(
            code=code,
            message=message,
            type_=type_,
            doc_url=doc_url,
            param=param,
        )

        error_detail.additional_properties = d
        return error_detail

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
