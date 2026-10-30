"""Turn an operator sentence into the section, form, and focus to show."""

from __future__ import annotations

import re
from dataclasses import dataclass

from .snapshot import describe, names_of

SECTIONS = (
    "overview",
    "vms",
    "volumes",
    "ceph",
    "networks",
    "storage",
    "operations",
    "audit",
    "compliance",
)

_KEYWORDS = (
    ("virtual machines", "vms"),
    ("virtual machine", "vms"),
    ("compliance", "compliance"),
    ("operations", "operations"),
    ("operation", "operations"),
    ("networks", "networks"),
    ("network", "networks"),
    ("volumes", "volumes"),
    ("volume", "volumes"),
    ("storage", "storage"),
    ("overview", "overview"),
    ("audit", "audit"),
    ("ceph", "ceph"),
    ("vms", "vms"),
    ("vm", "vms"),
)

_KIND_SECTION = {
    "vm": "vms",
    "volume": "volumes",
    "network": "networks",
    "ceph": "ceph",
    "filesystem": "ceph",
    "object": "ceph",
    "node": "storage",
}

_KIND_FORM = {
    "vm": "vm",
    "volume": "volume",
    "network": "network",
    "ceph": "ceph",
    "filesystem": "ceph",
    "object": "ceph",
    "node": "storage",
}

_MIGRATE = re.compile(
    r"\bmigrate\b(?:\s+vm)?\s+(\S+)\s+(?:to|onto)\s+(\S+)",
    re.IGNORECASE,
)
_POWER = re.compile(r"\b(start|stop)\b(?:\s+vm)?\s+(\S+)", re.IGNORECASE)
_TOKEN = re.compile(r"[A-Za-z0-9_.:@-]+")
_FILLER = re.compile(
    r"\b(show|me|the|please|open|list|all|a|an|for|of|that|are|is)\b",
    re.IGNORECASE,
)


def as_bool(value: object) -> bool:
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() in {"1", "true", "yes", "on"}


def safe_token(value: str) -> bool:
    return bool(_TOKEN.fullmatch(value.strip()))


def normalize_section(section: str) -> str:
    text = section.strip().lower().replace("_", " ")
    aliases = {
        "overview": "overview",
        "home": "overview",
        "vm": "vms",
        "vms": "vms",
        "virtual machine": "vms",
        "virtual machines": "vms",
        "volume": "volumes",
        "volumes": "volumes",
        "ceph": "ceph",
        "network": "networks",
        "networks": "networks",
        "storage": "storage",
        "operation": "operations",
        "operations": "operations",
        "audit": "audit",
        "compliance": "compliance",
    }
    return aliases.get(text, "overview")


def form_for(section: str) -> str:
    return {
        "overview": "none",
        "vms": "vm",
        "volumes": "volume",
        "ceph": "ceph",
        "networks": "network",
        "storage": "storage",
        "operations": "operations",
        "audit": "audit",
        "compliance": "compliance",
    }[normalize_section(section)]


def _clean(token: str) -> str:
    return token.strip(".,!?:;\"'`")


@dataclass
class Scene:
    section: str = "overview"
    form: str = "none"
    focus: str = ""
    focus_kind: str = ""
    filter: str = ""
    notice: str = ""
    reply: str = ""
    power: str = ""
    migrate_to: str = ""


def _help(section: str, focus: str) -> Scene:
    return Scene(
        section=normalize_section(section),
        form=form_for(section),
        focus=focus,
        reply=(
            "I can show overview, VMs, volumes, Ceph, networks, storage, "
            "operations, audit, or compliance. Name a VM to open its form. "
            "Start, stop, and migrate are sent after you tick confirm."
        ),
    )


def _keyword(text: str) -> tuple[str, str] | None:
    for phrase, section in _KEYWORDS:
        match = re.search(rf"\b{re.escape(phrase)}\b", text, re.IGNORECASE)
        if match is None:
            continue
        rest = text[: match.start()] + " " + text[match.end() :]
        rest = _FILLER.sub(" ", rest)
        return section, " ".join(rest.split())
    return None


def _known_vm(snap: dict, name: str) -> str | None:
    wanted = name.strip().lower()
    for lowered, kind, display in names_of(snap):
        if kind == "vm" and lowered == wanted:
            return display
    return None


def recorded_command(kind: str, target: str, argument: str) -> str | None:
    kind = kind.strip().lower()
    target = target.strip()
    argument = argument.strip()
    if kind in {"", "none"} and argument:
        kind = "migrate"
    if not target or not safe_token(target):
        return None
    if kind == "start":
        return f"kctl start vm {target}"
    if kind == "stop":
        return f"kctl stop vm {target}"
    if kind == "migrate":
        if not argument or not safe_token(argument):
            return None
        return f"kctl migrate vm {target} --target-node {argument}"
    return None


def operation_call(kind: str, target: str, argument: str) -> tuple[str, dict[str, str]] | None:
    """kcore_operation action and spec for a command this console is allowed to send."""
    kind = kind.strip().lower()
    target = target.strip()
    argument = argument.strip()
    if recorded_command(kind, target, argument) is None:
        return None
    if kind == "start":
        return "set-vm-state", {"name": target, "state": "running"}
    if kind == "stop":
        return "set-vm-state", {"name": target, "state": "stopped"}
    if kind == "migrate" or (kind in {"", "none"} and argument):
        return "migrate-vm", {"name": target, "targetNode": argument}
    return None


def stage_notice(kind: str, target: str, argument: str, confirm: object) -> tuple[str, str, str]:
    """Return notice, power, migrate_to. Confirm is what allows the send."""
    kind = kind.strip().lower()
    target = target.strip()
    argument = argument.strip()
    if kind in {"", "none"} and argument:
        kind = "migrate"
    if kind in {"start", "stop"} and argument:
        notice = "Choose start, stop, or a migrate target, not both. Nothing was sent."
        return notice, "", ""
    command = recorded_command(kind, target, argument)
    if command is None:
        if not target:
            notice = "Select a VM first. Nothing was sent."
        else:
            notice = "That name cannot go on a kctl line. Nothing was sent."
        return notice, kind if kind in {"start", "stop"} else "", argument if kind == "migrate" else ""
    if not as_bool(confirm):
        notice = f"Ready to send `{command}`. Tick confirm. The cluster is unchanged."
        power = kind if kind in {"start", "stop"} else ""
        migrate_to = argument if kind == "migrate" else ""
        return notice, power, migrate_to
    notice = f"Sending `{command}`."
    return notice, "", ""


def interpret(
    message: str,
    snap: dict,
    *,
    section: str = "overview",
    focus: str = "",
    filter_text: str = "",
) -> Scene:
    text = " ".join(message.split())
    if not text:
        return _help(section, focus)

    migrate = _MIGRATE.search(text)
    if migrate is not None:
        vm_name = _clean(migrate.group(1))
        node = _clean(migrate.group(2))
        known = _known_vm(snap, vm_name)
        focus_name = known or vm_name
        command = recorded_command("migrate", focus_name, node)
        if known is None:
            reply = f"No VM named {vm_name}."
            notice = reply
        elif command is None:
            reply = "That migrate target cannot go on a kctl line."
            notice = reply
        else:
            reply = (
                f"I can move {focus_name} to {node}. Tick confirm to send `{command}`."
            )
            notice = reply
        return Scene(
            section="vms",
            form="vm",
            focus=focus_name,
            focus_kind="vm",
            notice=notice,
            reply=reply,
            migrate_to=node if command else "",
        )

    power = _POWER.search(text)
    if power is not None:
        verb = power.group(1).lower()
        vm_name = _clean(power.group(2))
        known = _known_vm(snap, vm_name)
        if known is not None:
            command = recorded_command(verb, known, "")
            reply = f"I can {verb} {known}. Tick confirm to send `{command}`."
            return Scene(
                section="vms",
                form="vm",
                focus=known,
                focus_kind="vm",
                notice=reply,
                reply=reply,
                power=verb,
            )

    for name, kind, display in names_of(snap):
        if re.search(
            rf"(?<![A-Za-z0-9_.:@-]){re.escape(name)}(?![A-Za-z0-9_.:@-])",
            text,
            re.IGNORECASE,
        ):
            section_name = _KIND_SECTION[kind]
            reply = f"Showing {display}."
            return Scene(
                section=section_name,
                form=_KIND_FORM[kind],
                focus=display,
                focus_kind=kind,
                reply=reply,
            )

    found = _keyword(text)
    if found is not None:
        section_name, rest = found
        reply = f"Showing {section_name}."
        if rest:
            reply = f"{reply} Filter: {rest}."
        return Scene(
            section=section_name,
            form=form_for(section_name),
            filter=rest,
            reply=reply,
        )

    kept = Scene(
        section=normalize_section(section),
        form=form_for(section),
        focus=focus,
        filter=filter_text,
    )
    kept.reply = _help(section, focus).reply
    return kept


def scene_from_section(section: str, *, filter_text: str = "", focus: str = "") -> Scene:
    section_name = normalize_section(section)
    reply = f"Showing {section_name}."
    if filter_text.strip():
        reply = f"{reply} Filter: {filter_text.strip()}."
    return Scene(
        section=section_name,
        form=form_for(section_name),
        focus=focus,
        filter=filter_text.strip(),
        reply=reply,
    )


def scene_from_focus(snap: dict, kind: str, name: str) -> Scene:
    kind_name = kind.strip().lower()
    if kind_name not in _KIND_SECTION:
        return Scene(
            reply=f"Unknown kind {kind}. Use vm, volume, network, ceph, filesystem, object, or node.",
        )
    display = name.strip()
    for lowered, indexed_kind, indexed_name in names_of(snap):
        if indexed_kind == kind_name and lowered == display.lower():
            display = indexed_name
            break
    selected = describe(snap, kind=kind_name, name=display, form=_KIND_FORM[kind_name])
    missing = selected["line1"].startswith("No ")
    return Scene(
        section=_KIND_SECTION[kind_name],
        form=_KIND_FORM[kind_name],
        focus=display,
        focus_kind=kind_name,
        reply=selected["line1"] if missing else f"Showing {display}.",
        notice=selected["line1"] if missing else "",
    )
