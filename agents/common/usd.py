from __future__ import annotations


def usd_str(value: str) -> str:
    """`value` as a well-formed USD double-quoted string literal.

    Specialists interpolate brief-derived text (e.g. the CLI-supplied biome
    descriptor) into layer source. A raw value carrying a quote, backslash, or
    newline would close the literal early and the trailing text would parse as
    prims — corrupting the layer and, worse, letting one specialist's free-form
    input forge a prim the validator attributes to another. Escape the
    USD-significant characters so any input serializes to exactly one literal.
    Backslash is escaped first so the escapes introduced below aren't re-escaped.
    """
    escaped = (
        value.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
    )
    return f'"{escaped}"'
