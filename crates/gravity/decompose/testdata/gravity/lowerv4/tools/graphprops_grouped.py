"""The census side of props/group.rs's key loop (V4R-128, M3-GROUPED; split out of graphprops.py for its budget).

A keyed group's key names its hash arm; a sorted group / Unique's form K (runs/group.rs: keyed.hashagg's end, the records
emitted in key order) names each key's hash arm and its order arm. The arm: rep.code when the key is a code-lane
expression, rep.hosted for a tid key (D387), else the key kind's own (graphprops.key_arm; a float key's word arm: M6 L0-m keeps its datum).
"""
import sys


def key_arms(walk, gr, key, producer, packed, keyed):
    """add group `gr`'s key `key`'s arm kinds to walk.K (graphprops.Walk.group, per key)."""
    gp = sys.modules[type(walk).__module__]   # graphprops, however the caller loaded it
    sorted_k = walk.group_kind(gr) in ("runs.group", "runs.unique") and walk.grouped(gr)
    if not (keyed or sorted_k):
        return
    image = key["image"]
    if image is None and key["kind"] == "Fp128":
        image = gp.image_of_type(key["type"])
    e = key["expr"]
    c = producer is not None and walk.code_expr(producer, e, walk.last_project(producer))
    # props.rs hosted_group_key: a tid key (D387) keys through the hosted arm (a float key by its word image since M6 L0-m)
    hosted = key["type"] == 27
    for acc in ("hash", "order") if sorted_k else ("hash",):
        arm = f"rep.code.{acc}" if c else f"rep.hosted.{acc}" if hosted else gp.key_arm(key["kind"], image, packed, acc)
        walk.K.add("rep.packed.order" if arm == "rep.packed.hash" else arm)  # M6-CLOSE (a'): a packed lane key's order word
