from voice_clone.hfp import at


def test_parse_final_results():
    assert at.parse_line("OK") == at.Ok()
    assert at.parse_line("ERROR") == at.Error("ERROR")
    assert at.parse_line("+CME ERROR: 3") == at.Error("+CME ERROR: 3")
    assert at.parse_line("NO CARRIER") == at.Error("NO CARRIER")


def test_parse_ring_and_clip():
    assert at.parse_line("RING") == at.Ring()
    assert at.parse_line('+CLIP: "+919876543210",145') == at.Clip("+919876543210", 145)
    assert at.parse_line('+CLIP: "9876543210",129,,,"Mom",0') == at.Clip("9876543210", 129)
    assert at.parse_line('+CLIP: "",128') == at.Clip("", 128)
    assert at.parse_line('+CLIP:"123"') == at.Clip("123", None)


def test_parse_indicators():
    test = at.parse_line(
        '+CIND: ("service",(0,1)),("call",(0,1)),("callsetup",(0-3)),'
        '("callheld",(0-2)),("signal",(0-5)),("roam",(0,1)),("battchg",(0-5))'
    )
    assert test == at.CindTest(
        ("service", "call", "callsetup", "callheld", "signal", "roam", "battchg")
    )
    assert at.parse_line("+CIND: 1,0,0,0,4,0,5") == at.CindRead((1, 0, 0, 0, 4, 0, 5))
    assert at.parse_line("+CIEV: 3,1") == at.Ciev(3, 1)
    assert at.parse_line("+CIEV:2,0") == at.Ciev(2, 0)


def test_parse_features_codecs_and_calls():
    assert at.parse_line("+BRSF: 871") == at.Brsf(871)
    assert at.parse_line("+BCS: 2") == at.Bcs(2)
    assert at.parse_line("+CHLD: (0,1,1x,2,2x,3)") == at.Chld(("0", "1", "1x", "2", "2x", "3"))
    assert at.parse_line('+CLCC: 1,1,4,0,0,"+911234",145') == at.Clcc(1, 1, 4, "+911234")


def test_malformed_lines_are_unknown_not_crashes():
    assert at.parse_line("+CIEV: x,1") == at.Unknown("+CIEV: x,1")
    assert at.parse_line("+BRSF: lots") == at.Unknown("+BRSF: lots")
    assert at.parse_line("+XYZ: 1") == at.Unknown("+XYZ: 1")
    assert at.parse_line("hello") == at.Unknown("hello")


def test_line_reader_handles_split_and_joined_reads():
    reader = at.LineReader()
    assert reader.feed(b"\r\n+BR") == []
    assert reader.feed(b"SF: 871\r\n\r\nOK\r\n\r\nRI") == ["+BRSF: 871", "OK"]
    assert reader.feed(b"NG\r\n") == ["RING"]


def test_line_reader_bounds_unterminated_input():
    reader = at.LineReader()
    reader.feed(b"x" * (at.MAX_LINE + 10))
    assert reader.feed(b"\r\nOK\r\n") == ["OK"]


def test_command_encoding():
    assert at.command("ATA") == b"ATA\r"
