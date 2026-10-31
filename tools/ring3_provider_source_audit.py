#!/usr/bin/env python3
"""Source-boundary review for the staged Ring3 provider Job-accounting cutover.

This is a *static review aid*, never proof of build, kernel identity, durable
billing or lifecycle correctness. Invoke explicitly in Codex after compilation
and before KVM fault injection. It makes zero network or build invocations.
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class Finding:
    file: str
    check: str
    detail: str

    def __str__(self) -> str:
        return f"{self.file}: {self.check}: {self.detail}"


def source(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def lexical_block(text: str, needle: str) -> str:
    """Extract the contiguous Rust braces following an exact function header.

    Not a Rust parser. Escaped strings, comments and macros can contain braces;
    the returned text is useful only for checking obvious source-order drift.
    Compiler and actual kernel behavior must still be checked separately.
    """
    pos = text.find(needle)
    if pos == -1:
        raise ValueError("function marker absent: " + needle)
    start = text.find("{", pos)
    if start == -1:
        raise ValueError("no function body: " + needle)
    level = 0
    for index in range(start, len(text)):
        if text[index] == "{":
            level += 1
        elif text[index] == "}":
            level -= 1
            if level == 0:
                return text[start : index + 1]
    raise ValueError("unbalanced function: " + needle)


def ordered(haystack: str, *markers: str) -> bool:
    positions = [haystack.find(marker) for marker in markers]
    return -1 not in positions and positions == sorted(positions) and len(set(positions)) == len(markers)


def check(has: bool, path: str, name: str, detail: str, findings: list[Finding]) -> None:
    if not has:
        findings.append(Finding(path, name, detail))


def review_rootd(findings: list[Finding]) -> None:
    path = "services/rootd/src/provider_control.rs"
    text = source(path)
    body = lexical_block(text, "pub(super) fn execute(")
    check(ordered(body,
                  "exact_accounting_fields(request)?",
                  "authorize_sender_slow_grant(",
                  "kind(service)?",
                  "req.provider_service!=service",
                  "jobs::process_job(req.client_pid)",
                  "book.charge(ticket"),
          path, "charge-order", "exact grant/Job checks must precede charge", findings)
    check("book.release(owner,token" in body, path, "refund-owner",
          "refund must use exact owner/token entry", findings)
    check("provider_wire::encode_vec(ack)" in body, path, "canonical-reply",
          "ACK must match provider wire codec", findings)
    path = "services/rootd/src/provider_billing.rs"
    text = source(path)
    charge = lexical_block(text, "pub(super) fn charge(")
    close = lexical_block(text, "pub(super) fn release(")
    check(ordered(charge, "self.funded.push(Funded{", "match charge("),
          path, "charge-intent", "unknown charge intent must precede kernel call", findings)
    check(ordered(close, "State::ReleaseUnknown;", "refund(", "State::Settled;"),
          path, "refund-tombstone", "keep retryable terminal receipt", findings)
    check("State::Settled=>Err(ESTALE)" in charge,
          path, "spent-token", "released token may not charge again", findings)
    path = "services/rootd/src/main.rs"
    text = source(path)
    check("provider_control::execute(" in text, path, "dispatcher",
          "production supervisor must route provider op", findings)
    check(ordered(text,
                  "let funded_provider_exited =",
                  "service_checkpoints.providers.owner_exit_pid(",
                  "service_checkpoints.health.forget_pid(event.pid);"),
          path, "exit-drain", "kernel EXIT should drain before lease cleanup", findings)


def review_service(path: str, findings: list[Finding]) -> None:
    text = source(path)
    if path.endswith("/netd/src/main.rs"):
        sock = lexical_block(text, "fn handle_socket(")
        pair = lexical_block(text, "fn handle_socketpair(")
        close = lexical_block(text, "fn handle_close(")
        accept = lexical_block(text, "fn handle_accept(")
        for label, body, markers in [
            ("inet-charge", sock, ("provider_accounting::charge_before_publish(", "state.inet_sockets.insert(")),
            ("unix-charge", sock, ("provider_accounting::charge_before_publish(", "state.sockets.insert(")),
            ("socketpair-charge", pair, ("provider_accounting::charge_before_publish(left", "provider_accounting::charge_before_publish(right", "state.sockets.insert(")),
            ("close-refund", close, ("retire_unix_socket(", "provider_accounting::retire_final(")),
            ("accept-charge", accept, ("provider_accounting::charge_before_publish(accepted", "call_net_broker(")),
        ]:
            check(ordered(body, *markers), path, label,
                  "client Job charge before public socket identity; close refund after retirement",
                  findings)
        check("socket.refs > 1" in close and "socket.refs -= 1" in close,
              path, "dup-guard", "dup references must not retire charge early", findings)
    elif path.endswith("/vfsd/src/state_storage.rs"):
        body = lexical_block(text, "fn open_remote_checkpointed(")
        check(ordered(body, "self.bill_open_description(", "self.checkpoint_open_description("),
              path, "vfs-charge", "charge before successful OPEN checkpoint", findings)
        check("self.refund_open_description(id)" in body,
              path, "failed-open-refund", "failed checkpoint cannot leak unexposed debit", findings)
    elif path.endswith("/vfsd/src/state_files.rs"):
        body = lexical_block(text, "fn vfs_close(")
        check("self.refund_open_description(request.remote_id)" in body,
              path, "vfs-close", "final tombstone must refund", findings)
    elif path.endswith("/vfsd/src/state_requests.rs"):
        body = lexical_block(text, "fn handle_vfs_request(")
        check("!self.has_funded_open_description(" in body,
              path, "restart-rebind", "restored unbilled handle must refuse data ops", findings)


def review_runtime(findings: list[Finding]) -> None:
    path = "libs/rustos-svc-runtime/src/provider_client.rs"
    text = source(path)
    check("pub fn dispatch_charge(" in text and "pub fn dispatch_release(" in text,
          path, "replay-request", "same ticket is reused on ambiguous RPC", findings)
    check(ordered(text, "self.ledger", "pub fn call_rootd(") if "self.ledger" in text else
          "pub fn call_rootd(" in text, path, "slow-control", "service runtime owns explicit control RPC", findings)
    check("use rustos_user_abi::linux;" not in text, path, "no_std-feature",
          "svc-runtime cannot assume optional linux-compat-state feature", findings)
    path = "libs/rustos-component-model/src/provider_wire.rs"
    text = source(path)
    check("if self.operation==Operation::Charge" in text and "self.client_job!=0" in text,
          path, "untrusted-job", "charge request cannot select JobId", findings)


def main() -> int:
    global ROOT
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    ROOT = args.root.resolve()
    findings: list[Finding] = []
    try:
        review_rootd(findings)
        review_runtime(findings)
        for file in (
            "services/netd/src/main.rs",
            "services/vfsd/src/state_storage.rs",
            "services/vfsd/src/state_files.rs",
            "services/vfsd/src/state_requests.rs",
        ):
            review_service(file, findings)
    except (OSError, ValueError) as error:
        print("source audit unavailable:", error, file=sys.stderr)
        return 2
    if findings:
        for finding in findings:
            print(finding, file=sys.stderr)
        print(f"{len(findings)} source-boundary mismatch(es)", file=sys.stderr)
        return 1
    print("source-boundary markers present; compile/runtime validity NOT checked")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
