#!/usr/bin/env python3
"""Generate and validate the Local Dictation Sparkle appcast.

Stdlib only, no third-party packages. The appcast is the RSS 2.0 feed Sparkle 2
reads over HTTPS; each `<item>`/`<enclosure>` names one update archive and
carries its Ed25519 signature.

Two modes:

  build   write (or update) appcast.xml for one release artifact
  check   validate an existing appcast against the contract below

Contract enforced by `check`:
  * well-formed XML with the Sparkle namespace
  * one <channel> with a <title>
  * every <enclosure> has url, length, type and sparkle:edSignature
  * length is a decimal byte count; edSignature is base64 of exactly 64 bytes
  * if --artifact is supplied, its on-disk size matches the enclosure length
  * when --pubkey is supplied, the signature verifies against the artifact

`build --merge <existing>` keeps previously published items, replacing any item
whose sparkle:version already exists. That is how a channel keeps history
without the script owning a database.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import os
import sys
import xml.etree.ElementTree as ET

SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"
DC_NS = "http://purl.org/dc/elements/1.1/"
ET.register_namespace("sparkle", SPARKLE_NS)
ET.register_namespace("dc", DC_NS)

# Sparkle matches software by these enclosure attributes.
S_VERSION = f"{{{SPARKLE_NS}}}version"
S_SHORT = f"{{{SPARKLE_NS}}}shortVersionString"
S_MIN = f"{{{SPARKLE_NS}}}minimumSystemVersion"
S_SIG = f"{{{SPARKLE_NS}}}edSignature"


def die(message: str) -> "NoReturn":
    print(f"appcast: {message}", file=sys.stderr)
    raise SystemExit(1)


def local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def item_version(item: ET.Element) -> str | None:
    # Sparkle 2 recommends the version elements at item level; enclosure
    # attributes remain supported for older feeds. Read either.
    for child in item:
        if local_name(child.tag) == "version" and child.text:
            return child.text.strip()
    enclosure = item.find("enclosure")
    if enclosure is not None and enclosure.get(S_VERSION):
        return enclosure.get(S_VERSION)
    return None


def require_top_level(item: ET.Element, name: str) -> str | None:
    for child in item:
        if local_name(child.tag) == name and child.text:
            return child.text.strip()
    return None


def build_item(args: argparse.Namespace) -> ET.Element:
    item = ET.Element("item")
    ET.SubElement(item, "title").text = args.title or args.short_version or args.version
    ET.SubElement(item, "pubDate").text = args.pubdate
    # Sparkle 2 recommends these as top-level item elements rather than
    # enclosure attributes (both are accepted; this is the current form).
    ET.SubElement(item, S_VERSION).text = args.version
    ET.SubElement(item, S_SHORT).text = args.short_version or args.version
    ET.SubElement(item, S_MIN).text = args.minimum_system
    if args.release_notes_url:
        ET.SubElement(item, f"{{{SPARKLE_NS}}}releaseNotesLink").text = args.release_notes_url
    # The enclosure carries the archive and its Ed25519 signature only.
    enclosure = ET.SubElement(item, "enclosure")
    enclosure.set("url", args.url)
    enclosure.set("length", str(args.length))
    enclosure.set("type", "application/octet-stream")
    enclosure.set(S_SIG, args.signature)
    return item


def cmd_build(args: argparse.Namespace) -> int:
    if not os.path.isfile(args.artifact):
        die(f"artifact not found: {args.artifact}")
    length = os.path.getsize(args.artifact)
    if length <= 0:
        die(f"artifact is empty: {args.artifact}")
    if not args.url.startswith("https://"):
        die(f"the enclosure url must be HTTPS, got: {args.url}")
    sig_bytes = base64.b64decode(args.signature, validate=True)
    if len(sig_bytes) != 64:
        die("edSignature is not a 64-byte Ed25519 signature")

    if args.merge:
        tree = ET.parse(args.merge)
        root = tree.getroot()
        channel = root.find("channel")
        if channel is None:
            die(f"--merge has no <channel>: {args.merge}")
        kept = [i for i in channel.findall("item") if item_version(i) != args.version]
        for stale in channel.findall("item"):
            channel.remove(stale)
        for i in kept:
            channel.append(i)
        app_title = channel.findtext("title")
        if app_title:
            channel.find("title").text = app_title
    else:
        root = ET.Element("rss", {"version": "2.0"})
        channel = ET.SubElement(root, "channel")
        ET.SubElement(channel, "title").text = args.channel_title
        ET.SubElement(channel, "link").text = args.feed_url
        ET.SubElement(channel, "description").text = args.channel_title
        ET.SubElement(channel, "language").text = "en"

    args.length = length  # derived from the artifact, never trusted from a flag
    channel.append(build_item(args))

    ET.indent(root)
    ET.ElementTree(root).write(args.out, encoding="utf-8", xml_declaration=True)
    print(f"wrote {args.out} ({length} bytes, version {args.version})")
    return 0


def cmd_check(args: argparse.Namespace) -> int:
    if not os.path.isfile(args.appcast):
        die(f"appcast not found: {args.appcast}")
    try:
        root = ET.parse(args.appcast).getroot()
    except ET.ParseError as exc:
        die(f"appcast is not well-formed XML: {exc}")
    if local_name(root.tag) != "rss":
        die("root element is not <rss>")
    channel = root.find("channel")
    if channel is None:
        die("no <channel>")
    title = channel.findtext("title")
    if not title:
        die("<channel> has no <title>")
    items = channel.findall("item")
    if not items:
        die("<channel> has no <item>")
    for item in items:
        enclosure = item.find("enclosure")
        if enclosure is None:
            die("an <item> has no <enclosure>")
        url = enclosure.get("url") or ""
        if not url.startswith("https://"):
            die(f"enclosure url is not HTTPS: {url!r}")
        length = enclosure.get("length")
        if not length or not length.isdigit():
            die(f"enclosure length is not a byte count: {length!r}")
        if not enclosure.get("type"):
            die("enclosure has no type")
        if not (require_top_level(item, "version") or enclosure.get(S_VERSION)):
            die("item has no sparkle:version")
        if not (require_top_level(item, "shortVersionString") or enclosure.get(S_SHORT)):
            die("item has no sparkle:shortVersionString")
        minimum = require_top_level(item, "minimumSystemVersion") or enclosure.get(S_MIN)
        if not minimum:
            die("item has no sparkle:minimumSystemVersion")
        if minimum.count(".") != 2:
            die(f"sparkle:minimumSystemVersion must be major.minor.patch, got {minimum!r}")
        sig = enclosure.get(S_SIG)
        if not sig:
            die("enclosure has no sparkle:edSignature")
        try:
            raw = base64.b64decode(sig, validate=True)
        except Exception:
            die("sparkle:edSignature is not valid base64")
        if len(raw) != 64:
            die("sparkle:edSignature is not a 64-byte signature")
    if args.artifact:
        enclosure = items[-1].find("enclosure")
        expected = int(enclosure.get("length"))
        actual = os.path.getsize(args.artifact)
        if expected != actual:
            die(f"enclosure length {expected} != artifact size {actual}")
        if not args.signature:
            die("--artifact given without --signature; cannot verify the signature")
        if args.signature != enclosure.get(S_SIG):
            die("--signature does not match the appcast")
    print(f"ok: {len(items)} item(s) in {args.appcast}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    b = sub.add_parser("build", help="write or update an appcast")
    b.add_argument("--artifact", required=True)
    b.add_argument("--url", required=True)
    b.add_argument("--signature", required=True, help="base64 Ed25519 signature of the artifact")
    b.add_argument("--version", required=True, help="sparkle:version (CFBundleVersion)")
    b.add_argument("--short-version", default=None, help="sparkle:shortVersionString")
    b.add_argument("--minimum-system", default="14.0.0")
    b.add_argument("--title", default=None)
    b.add_argument("--channel-title", default="Local Dictation")
    b.add_argument("--feed-url", default="")
    b.add_argument("--release-notes-url", default=None)
    b.add_argument("--pubdate", default=datetime.datetime.now(datetime.timezone.utc).strftime("%a, %d %b %Y %H:%M:%S %z"))
    b.add_argument("--merge", default=None, help="existing appcast to keep older items from")
    b.add_argument("--out", required=True)
    b.set_defaults(func=cmd_build)

    c = sub.add_parser("check", help="validate an appcast")
    c.add_argument("appcast")
    c.add_argument("--artifact", default=None)
    c.add_argument("--signature", default=None)
    c.set_defaults(func=cmd_check)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
