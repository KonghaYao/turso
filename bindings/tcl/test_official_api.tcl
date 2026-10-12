if {[llength $argv] < 2 || [llength $argv] > 3} {
    error "usage: test_official_api.tcl LIBRARY TEST_API_ENABLED ?OFFICIAL_TESTER?"
}

proc assert_equal {actual expected label} {
    if {$actual ne $expected} {
        error "$label: expected <$expected>, got <$actual>"
    }
    puts "PASS $label"
}

set library [file normalize [lindex $argv 0]]
set enabled [lindex $argv 1]
load $library Tursotcl

assert_equal [info commands sqlite3_shutdown] {} "no fake shutdown"
assert_equal [info commands install_malloc_faultsim] {} "no fake allocator"
sqlite3 smoke :memory:
assert_equal [smoke eval {SELECT 1+2}] 3 "normal adapter SQL"
smoke close

if {!$enabled} {
    assert_equal [info commands sqlite3_test_control_pending_byte] {} "default has no pending command"
    assert_equal [info exists ::sqlite_pending_byte] 0 "default has no pending variable"
    assert_equal [info exists ::sqlite_options] 0 "default has no test capabilities"
    puts "PASS default adapter"
    exit 0
}

assert_equal [info commands sqlite3_test_control_pending_byte] sqlite3_test_control_pending_byte "real pending command"
set original [sqlite3_test_control_pending_byte 0]
try {
    assert_equal $::sqlite_pending_byte $original "initial engine value"
    assert_equal [sqlite3_test_control_pending_byte 65536] $original "setter returns previous value"
    assert_equal $::sqlite_pending_byte 65536 "variable reads changed engine value"
    assert_equal [sqlite3_test_control_pending_byte 0] 65536 "zero queries without mutation"
    foreach arguments {{} {1 2} {not-an-integer} {4294967296} {-4294967296}} {
        assert_equal [catch {sqlite3_test_control_pending_byte {*}$arguments}] 1 "reject arguments <$arguments>"
        assert_equal $::sqlite_pending_byte 65536 "invalid arguments leave engine unchanged"
    }
    assert_equal [catch {set ::sqlite_pending_byte 17}] 1 "variable is read-only"
    assert_equal $::sqlite_pending_byte 65536 "failed variable write preserves engine"
    assert_equal [sqlite3_test_control_pending_byte -1] 65536 "signed argument converts to uint32"
    assert_equal $::sqlite_pending_byte -1 "variable uses official signed int representation"
    assert_equal [sqlite3_test_control_pending_byte 0] -1 "old uint32 value returns signed int"
    assert_equal [sqlite3_test_control_pending_byte 0xffffffff] -1 "Tcl integer upper unsigned boundary"
    assert_equal [sqlite3_test_control_pending_byte -2147483649] -1 "official Tcl accepts negative uint32 boundary"
    assert_equal $::sqlite_pending_byte 2147483647 "negative boundary wraps as official Tcl int"
    assert_equal [sqlite3_test_control_pending_byte 65536] 2147483647 "restore test offset"
    interp create peer
    try {
        peer eval [list load $library Tursotcl]
        assert_equal [peer eval {set ::sqlite_pending_byte}] 65536 "second interpreter shares engine"
        assert_equal [peer eval {sqlite3_test_control_pending_byte 1048576}] 65536 "second interpreter updates engine"
        assert_equal $::sqlite_pending_byte 1048576 "first interpreter sees external engine update"
        assert_equal [sqlite3_test_control_pending_byte 65536] 1048576 "first interpreter returns shared previous value"
        assert_equal [peer eval {set ::sqlite_pending_byte}] 65536 "second interpreter sees external engine update"
    } finally {
        interp delete peer
    }
    unset ::sqlite_pending_byte
    assert_equal $::sqlite_pending_byte 65536 "unset recreates engine variable"
    assert_equal [catch {set ::sqlite_pending_byte 17}] 1 "recreated variable remains read-only"
} finally {
    sqlite3_test_control_pending_byte $original
}

sqlite3 capabilities :memory:
try {
    foreach {capability sql expected} {
        subquery {SELECT * FROM (SELECT 7)} 7
        compound {SELECT 1 UNION ALL SELECT 2} {1 2}
        view {CREATE VIEW v AS SELECT 3; SELECT * FROM v} 3
        trigger {CREATE TABLE t(x); CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET x=x+1; END; INSERT INTO t VALUES(4); SELECT x FROM t} 5
        pragma {PRAGMA page_size} 4096
        tempdb {CREATE TEMP TABLE tt(x); INSERT INTO tt VALUES(6); SELECT x FROM tt} 6
        attach {ATTACH ':memory:' AS other; CREATE TABLE other.a(x); INSERT INTO other.a VALUES(8); SELECT x FROM other.a; DETACH other} 8
    } {
        assert_equal $::sqlite_options($capability) 1 "advertised capability $capability"
        assert_equal [capabilities eval $sql] $expected "engine capability $capability"
    }
    assert_equal $::sqlite_options(memdebug) 0 "no SQLite malloc instrumentation"
    assert_equal [info exists ::sqlite_options(foreignkey)] 0 "unverified capability is not falsely disabled"
} finally {
    capabilities close
}

if {[llength $argv] == 3} {
    set tester [file normalize [lindex $argv 2]]
    set channel [file tempfile directory]
    close $channel
    file delete $directory
    file mkdir $directory
    set previous_directory [pwd]
    set previous_argv $argv
    set original [sqlite3_test_control_pending_byte 0]
    try {
        cd $directory
        set argv {}
        set argv0 [file join $directory official-api-probe.tcl]
        set testdir [file dirname $tester]
        set failed [catch {source $tester} message options]
        puts "OFFICIAL_TESTER $tester"
        puts "OFFICIAL_RESULT $failed $message"
        if {$failed} { puts [dict get $options -errorinfo] }
        assert_equal $failed 1 "official harness exposes next unsupported API"
        assert_equal $message {invalid command name "sqlite3_shutdown"} "next real blocker is lifecycle"
        assert_equal $::sqlite_pending_byte 65536 "official tester line 102 updates engine"
        puts "BLOCKED official tester: real shutdown/allocator lifecycle missing; no full-suite claim"
    } finally {
        if {[info commands db] ne {}} {catch {db close}}
        sqlite3_test_control_pending_byte $original
        set argv $previous_argv
        cd $previous_directory
        file delete -force $directory
    }
}
puts "PASS official pending-byte API probes"
