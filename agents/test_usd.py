from common.usd import usd_str


def test_usd_str_wraps_a_plain_value_in_double_quotes():
    assert usd_str("terran ridge") == '"terran ridge"'
    assert usd_str("") == '""'


def test_usd_str_escapes_each_significant_char():
    assert usd_str('say "hi"') == '"say \\"hi\\""'
    assert usd_str("line\nbreak") == '"line\\nbreak"'
    assert usd_str("carriage\rreturn") == '"carriage\\rreturn"'
    assert usd_str("tab\there") == '"tab\\there"'
    assert usd_str("back\\slash") == '"back\\\\slash"'


def test_usd_str_escapes_backslash_before_the_others():
    # A backslash abutting a quote must double the backslash FIRST, so the quote's own
    # escape backslash is never itself re-doubled. Quote-first ordering would turn '\"'
    # into four backslashes and strip the quote's escape, reopening the injection the
    # helper exists to close.
    assert usd_str('\\"') == '"' + "\\\\" + '\\"' + '"'
    assert usd_str("\\") == '"\\\\"'


def test_usd_str_escapes_all_five_significant_chars_in_one_value():
    assert usd_str('\\"\n\r\t') == '"' + "\\\\" + '\\"' + "\\n\\r\\t" + '"'
