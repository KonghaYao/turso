set text_byte_controls 0
if {[llength $argv] > 0 && [lindex $argv end] eq "--text-byte-controls"} {
    set text_byte_controls 1
    set argv [lrange $argv 0 end-1]
}

if {[llength $argv] == 1} {
    load [file normalize [lindex $argv 0]] Tursotcl
} elseif {[llength $argv] != 0 || [llength [info commands sqlite3]] == 0} {
    puts stderr "usage: tclsh test_function_types.tcl extension ?--text-byte-controls? | testfixture test_function_types.tcl ?--text-byte-controls?"
    exit 2
}

set failures 0
set checks 0

proc check {label sql expected} {
    incr ::checks
    if {[catch {db eval $sql} actual]} {
        puts "FAIL $label ERROR $actual"
        incr ::failures
    } elseif {$actual ne $expected} {
        puts "FAIL $label expected=[list $expected] actual=[list $actual]"
        incr ::failures
    } else {
        puts "PASS $label [list $actual]"
    }
}

proc numeric_text {} {
    return [string range x7 1 end]
}

sqlite3 db :memory:
db function numeric_text numeric_text
check numeric-text {SELECT typeof(numeric_text()), hex(numeric_text())} {text 37}

foreach {label body expected} {
    decimal-text {return [string range x7.5 1 end]} {text 372E35}
    exponent-text {return [string range x1e2 1 end]} {text 316532}
    leading-zero-text {return [string range x007 1 end]} {text 303037}
    signed-text {return [string range x+7 1 end]} {text 2B37}
    whitespace-text {return [string range {x 7 } 1 end]} {text 203720}
    empty-text {return [string range x 1 end]} {text {}}
    unicode-text {return [string range xé中 1 end]} {text C3A9E4B8AD}
    integer {return [expr {7}]} {integer 37}
    negative-integer {return [expr {-7}]} {integer 2D37}
    wide-integer {return [expr {9223372036854775807}]} {integer 39323233333732303336383534373735383037}
    minimum-integer {return [expr {-9223372036854775808}]} {integer 2D39323233333732303336383534373735383038}
    integral-double {return [expr {7.0}]} {real 372E30}
    fractional-double {return [expr {7.5}]} {real 372E35}
    boolean-true {return [expr {1 == 1}]} {integer 31}
    boolean-false {return [expr {1 == 0}]} {integer 30}
    boolean-word-true {return [expr {true}]} {text 74727565}
    boolean-word-false {return [expr {false}]} {text 66616C7365}
    boolean-text {return [string range xtrue 1 end]} {text 74727565}
    boolean-with-string {set value [expr {true}]; string length $value; return $value} {text 74727565}
    numeric-blob {return [binary format H* 37]} {blob 37}
    binary-blob {return [binary format H* 00ff4180]} {blob 00FF4180}
    empty-binary-format {return [binary format H* {}]} {text {}}
    blob-with-string {set value [binary format H* 37]; encoding convertto utf-8 $value; return $value} {text 37}
} {
    set function_name [string map {- _} $label]
    db function $function_name [list apply [list {} $body]]
    check $label "SELECT typeof(${function_name}()), hex(${function_name}())" $expected
}

proc identity {value} {
    return $value
}

db function identity identity
check null-control {SELECT typeof(NULL), NULL IS NULL, hex(NULL)} {null 1 {}}
check null-argument {SELECT typeof(identity(NULL)), hex(identity(NULL)), identity(NULL) IS NULL} {text {} 0}
check text-argument {SELECT typeof(identity('7')), hex(identity('7'))} {text 37}
check integer-argument {SELECT typeof(identity(7)), hex(identity(7))} {integer 37}
check double-argument {SELECT typeof(identity(7.0)), hex(identity(7.0))} {real 372E30}
check blob-argument {SELECT typeof(identity(x'00ff4180')), hex(identity(x'00ff4180'))} {blob 00FF4180}
check empty-blob-argument {SELECT typeof(identity(x'')), hex(identity(x''))} {blob {}}
check embedded-nul-argument {SELECT typeof(identity(CAST(x'410042' AS TEXT))), hex(identity(CAST(x'410042' AS TEXT)))} {text 410042}

if {$text_byte_controls} {
    db function modified_nul [list apply [list {} {return [format "A%cB" 0]}]]
    check modified-utf8-nul {SELECT typeof(modified_nul()), hex(modified_nul())} {text 41C08042}
    db function supplementary_unicode [list apply [list {} {return "\ud83d\ude42"}]]
    check supplementary-unicode {SELECT typeof(supplementary_unicode()), hex(supplementary_unicode())} {text EDA0BDEDB982}
}
db close
puts "RESULT checks=$checks failures=$failures"
exit [expr {$failures != 0}]
