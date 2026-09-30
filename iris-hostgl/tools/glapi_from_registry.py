#!/usr/bin/env python3
"""Build glapi.json from the Khronos OpenGL registry and our annotations.

  python3 iris-hostgl/tools/glapi_from_registry.py

glapi.json, which glshim.py generates both halves of the host GL wire protocol
from, is two things merged:

  khronos/gl.xml           the OpenGL API Registry (Khronos, Apache-2.0: see
                           khronos/README.md), pinned: every entry point's
                           declaration -- return type, parameter names and
                           types -- and every enumerant's value
  glapi_annotations.json   ours: the entry points the protocol carries and in
                           what order (the order IS the op numbering: append,
                           never reorder), how each argument crosses to the
                           host, the size tables, and where IRIX 6.5's own
                           declarations differ from the registry's

The registry describes today's OpenGL; IRIX 6.5 shipped an earlier one. Where
the two disagree in a way the guest's C must follow -- it is compiled against
IRIX's own <GL/gl.h> -- the annotations say so, with the reason
(irix_overrides), and entry points IRIX has that the registry never listed
come from them whole (irix_only). Everything else is taken as the registry
has it: parameter names (glDrawBuffer's `mode` is `buf` there), and types
that are the same C type under another name (GLclampf is GLfloat, GLvoid is
void). A merge that would silently change the protocol stops instead: a
function the registry lacks and the annotations do not supply, a parameter
count that differs, a size-table enumerant with no value.
"""
import json
import os
import re
import sys
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.abspath(__file__))


def fail(msg):
    raise SystemExit(f"glapi_from_registry.py: {msg}")


def decl(elem):
    """name, base type, pointer depth, const -- from a <proto> or <param>."""
    name = elem.find("name").text
    text = "".join(elem.itertext())
    before = text[: text.rfind(name)]
    ptr = before.count("*")
    const = "const" in before.replace("*", " ").split()
    ptype = elem.find("ptype")
    base = ptype.text if ptype is not None else re.sub(r"\bconst\b|\*", "", before).strip()
    return name, base, ptr, const


def load_registry(path):
    root = ET.parse(path).getroot()
    commands = {}
    for cmd in root.iter("command"):
        proto = cmd.find("proto")
        if proto is None:
            continue            # a reference in a <require> list, not a declaration
        name, base, ptr, const = decl(proto)
        ret = ("const " if const else "") + base + (" " + "*" * ptr if ptr else "")
        params = []
        for p in cmd.findall("param"):
            pname, pbase, pptr, pconst = decl(p)
            params.append({"type": pbase, "name": pname, "ptr": pptr, "const": pconst})
        commands[name] = {"ret": ret, "params": params}
    enums = {}
    for e in root.iter("enum"):
        if e.get("name") and e.get("value") and e.get("api") in (None, "gl"):
            enums.setdefault(e.get("name"), int(e.get("value"), 0))
    return commands, enums


def main():
    commands, reg_enums = load_registry(os.path.join(HERE, "khronos", "gl.xml"))
    ann = json.load(open(os.path.join(HERE, "glapi_annotations.json")))
    overrides = ann["irix_overrides"]
    irix_only = ann["irix_only"]

    functions = []
    for a in ann["functions"]:
        name = a["name"]
        if name in irix_only:
            src = irix_only[name]
            d = {"ret": src["ret"], "params": [dict(p) for p in src["params"]]}
        elif name in commands:
            d = {"ret": commands[name]["ret"], "params": [dict(p) for p in commands[name]["params"]]}
        else:
            fail(f"{name} is in the annotations but not in the registry or irix_only")
        for idx, change in overrides.get(name, {}).get("params", {}).items():
            d["params"][int(idx)].update(change)
        extra = a.get("params", [])
        if extra and len(extra) != len(d["params"]):
            fail(f"{name}: the registry has {len(d['params'])} parameters, the annotations {len(extra)}")
        for p, x in zip(d["params"], extra):
            p.update(x)
        names = {p["name"] for p in d["params"]}
        # Annotations name other parameters (counts, keys, formats): each must
        # still be one after the registry's renames.
        for p in d["params"]:
            refs = [p.get(k) for k in ("key", "format_param", "type_param", "target", "get", "typed", "pixelmap")]
            refs += [w for o in p.get("orders", []) for w in o] + list(p.get("mapquery", []))
            refs += re.findall(r"[A-Za-z_]\w*", str(p.get("count", "")))
            refs += [w for w in p.get("dims", []) if re.fullmatch(r"[A-Za-z_]\w*", str(w))]
            for r in refs:
                if isinstance(r, str) and r and r not in names:
                    fail(f"{name}: an annotation names `{r}`, which is not one of its parameters {sorted(names)}")
        functions.append({"name": name, "ret": d["ret"], "params": d["params"], "sync": a["sync"], "ext": a["ext"]})

    # Enumerant values, from the registry, for every name the tables use.
    needed = {e for t in ann["sizes"].values() for e in t} | set(ann["get_multi"])
    missing = sorted(e for e in needed if e not in reg_enums)
    if missing:
        fail(f"size-table enumerants the registry has no value for: {missing}")
    enums = {e: reg_enums[e] for e in sorted(needed)}

    out = {"functions": functions, "enums": enums, "extensions": ann["extensions"],
           "sizes": ann["sizes"], "get_multi": ann["get_multi"]}
    with open(os.path.join(HERE, "glapi.json"), "w") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    print(f"glapi.json: {len(functions)} entry points ({len(irix_only)} IRIX-only, "
          f"{len(overrides)} with IRIX overrides), {len(enums)} enumerants")


if __name__ == "__main__":
    main()
