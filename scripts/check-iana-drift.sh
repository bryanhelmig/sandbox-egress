#!/bin/sh
set -eu

ipv4_sha256=e3e39e76d00b1677335db8e9a805c7b9480ea2f4dc9e33f0b93cd3a905128d73
ipv6_sha256=775feea0621dec8735a44fbf30f762e721e8f0a1b3ab7eb341961a88cfce2139
iana_tmp_dir=$(mktemp -d)
trap 'rm -rf "$iana_tmp_dir"' EXIT HUP INT TERM

hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo "error: sha256sum or shasum is required" >&2
        return 2
    fi
}

check_registry() {
    registry_name=$1
    expected_sha256=$2
    registry_file="$iana_tmp_dir/$registry_name.csv"
    registry_url="https://www.iana.org/assignments/$registry_name/$registry_name-1.csv"

    curl -fsSL "$registry_url" -o "$registry_file"
    actual_sha256=$(hash_file "$registry_file")
    if [ "$actual_sha256" != "$expected_sha256" ]; then
        echo "IANA registry changed: $registry_name" >&2
        echo "review $registry_url before updating the pin" >&2
        echo "expected=$expected_sha256 actual=$actual_sha256" >&2
        return 1
    fi
    echo "IANA registry unchanged: $registry_name sha256=$actual_sha256"
}

result_code=0
check_registry iana-ipv4-special-registry "$ipv4_sha256" || result_code=1
check_registry iana-ipv6-special-registry "$ipv6_sha256" || result_code=1
exit "$result_code"
