<?php
class Greeter {
    public function greet($who) {
        return format_name($who);
    }
}

function format_name($s) {
    return "Hello " . $s;
}

function main() {
    (new Greeter())->greet("x");
}
