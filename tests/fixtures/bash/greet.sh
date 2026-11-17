#!/usr/bin/env bash
format_name() {
  echo "Hello $1"
}

greet() {
  format_name "$1"
}

main() {
  greet x
}
