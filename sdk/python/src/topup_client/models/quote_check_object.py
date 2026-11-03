from typing import Literal

QuoteCheckObject = Literal["quote_check"]

QUOTE_CHECK_OBJECT_VALUES: set[QuoteCheckObject] = {
    "quote_check",
}


def check_quote_check_object(value: str) -> QuoteCheckObject:
    if value in QUOTE_CHECK_OBJECT_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {QUOTE_CHECK_OBJECT_VALUES!r}")
