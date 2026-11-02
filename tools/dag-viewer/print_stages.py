"""Print a stage_pipeline document as text: each stage's DAG and the memo after each step.

    cargo run -q -p asap-devtools --bin stage_pipeline -- --workload w.json --out run.json
    python3 tools/dag-viewer/print_stages.py run.json               # stages and memo steps
    python3 tools/dag-viewer/print_stages.py run.json --enumerated  # every enumerated memo choice

`--max-enumerated N` on stage_pipeline raises how many memo choices it lists."""
import json, sys
from collections import Counter

d = json.load(open(sys.argv[1]))
CHILD_KEYS = {"child", "children", "inputs", "left", "right", "summary_input"}
DROP = {"schema", "output_names"}


def compact(v):
    return json.dumps(v, separators=(",", ":"))


def op_label(op):
    # {"NonASAP": {"Aggregate": {...}}} -> ("NonASAP.Aggregate", params, children)
    kind = next(iter(op)); inner = op[kind]
    if not isinstance(inner, dict):
        return f"{kind}.{inner}", "", []
    name = next(iter(inner)); body = inner[name]
    if not isinstance(body, dict):
        return f"{kind}.{name}", compact(body), []
    kids = [x for k in CHILD_KEYS & body.keys() for x in (body[k] if isinstance(body[k], list) else [body[k]])]
    params = {k: v for k, v in body.items() if k not in CHILD_KEYS | DROP and v not in (None, [], {})}
    return f"{kind}.{name}", compact(params), kids


def schema_cols(s):
    return ",".join(f["name"] for f in (s or {}).get("fields", []))


def print_dag(dag, indent="  "):
    print(f"{indent}roots: {compact(dag['roots'])}")
    for i, n in enumerate(dag["nodes"]):
        op = n.get("operator") or n.get("payload")
        name, params, kids = op_label(op)
        mat = n.get("materialization")
        mat = "" if mat in (None, "NotMaterialized") else f"  mat={compact(mat)}"
        cols = schema_cols(n.get("schema") or n.get("output_schema"))
        print(f"{indent}#{n.get('id', i)} {name} <- {kids}  [{cols}]{mat}")
        if params != "{}":
            print(f"{indent}    {params}")


# stage_pipeline --no-memo leaves the memo out.
memo = d.get("memo", {"steps": [], "enumeration": None})


def window_params(node):
    """PaneChain/SummaryMerge window parameters, which the memo labels omit."""
    name, body = next(iter(node["operator"].get("ASAP", {}).items()), (None, None))
    if name == "PaneChain":
        return name, f"[{body['pane_count']}x{body['pane_width_ms'] // 1000}s]"
    if name == "SummaryMerge":
        # A missing newest_panes means every input pane is merged.
        return name, f"[newest {body.get('newest_panes') or 'all'}]"
    return None


def with_window_params(label, root, stop):
    # Own nodes only: stop at nodes that belong to the input groups.
    found, stack, seen = [], [root], set()
    while stack:
        i = stack.pop()
        if i in seen or str(i) in stop:
            continue
        seen.add(i)
        params = window_params(memo["nodes"][i])
        if params:
            found.append(params)
        _, kids = op_label(memo["nodes"][i]["operator"])[1:]
        stack.extend(reversed([k for k in kids if isinstance(k, int)]))
    pos = 0
    for name, suffix in found:
        pos = label.find(name, pos)
        if pos < 0:
            break
        pos += len(name)
        label = label[:pos] + suffix + label[pos:]
    return label


def short_node(i):
    """One line per memo-table node: operator plus the parameters that tell plans apart."""
    name, _, kids = op_label(memo["nodes"][i]["operator"])
    op = memo["nodes"][i]["operator"]
    body = next(iter(op[next(iter(op))].values()))
    extra = []
    if isinstance(body, dict):
        if "family" in body:
            sketch = body["family"]["Sketch"][0]
            extra.append(f"{sketch['algorithm']}{compact(next(iter(sketch['params'].values())))}")
        if "range" in body:
            extra.append(f"{body['range']['secs']}s")
        q = body.get("query", {}).get("Quantile") if isinstance(body.get("query"), dict) else None
        if q:
            extra.append(f"q={q['q']}")
        for m in body.get("measures", []):
            extra.append(f"{m['kind']}(q={m.get('q')})")
    params = window_params(memo["nodes"][i])
    label = name.split(".")[1] + (params[1] if params else "")
    return f"{label} {' '.join(extra)}".strip(), [k for k in kids if isinstance(k, int)]


def print_memo_dag(roots):
    reachable, stack = set(), list(roots)
    while stack:
        i = stack.pop()
        if i not in reachable:
            reachable.add(i)
            stack.extend(short_node(i)[1])
    # The memo node table lists children before parents.
    for i in sorted(reachable):
        text, kids = short_node(i)
        root = f"   <- ROOT q{roots.index(i)}" if i in roots else ""
        print(f"    n{i} {text} <- {['n%d' % k for k in kids]}{root}")


if "--enumerated" in sys.argv:
    e = memo["enumeration"]
    if e is None:
        sys.exit("no memo enumeration: the document was written with --no-memo")
    print(f"Every choice of one candidate per reached group, at memo step {e['step']}: "
          f"{len(e['choices'])} listed, {compact(e['per_step'])}")
    for c in e["choices"]:
        mark = "  <== SELECTED" if c["selected"] else ""
        choice = " ".join(f"G{g}:c{i}" for g, i in c["choice"].items())
        print(f"\n{c['id']}  {choice}{mark}")
        print_memo_dag(c["roots"]) if c["roots"] is not None else print(f"    error: {c['error']}")
    sys.exit()

print("=" * 80, "\nSTAGE 0: frontends' DAG")
print_dag(d["stage0_query_language_frontends"]["dag"])

for step in memo["steps"]:
    print("=" * 80, f"\nMEMO after {step['step']}: {step['title']}")
    for g in step["groups"]:
        roots = f" ROOT of q{g['roots_of']}" if g["roots_of"] else ""
        label = with_window_params(g["label"], g["node"], set())
        print(f"  G{g['id']} {label}  children={g['children']} parents={g['parents']} queries={g['queries']}{roots}")
        for c in g["candidates"]:
            new = " NEW" if c["added"] and step["step"] != "stage0" else ""
            label = with_window_params(c["label"], c["node"], c["group_of_node"])
            print(f"      cand {c['index']} [{c['origin']}{new}] {label}  reads={c['children']} "
                  f"implements={c['implements']} budget_parts={c['accuracy_budget_parts']}")
        if g["candidates_capped"]:
            print(f"      ... {g['candidate_count']} total")
if "selected_choice" in memo:
    print(f"  selected choice (group -> cand): {compact(memo['selected_choice'])}")

s1 = d["stage1_logical_asap_optimization"]
print("=" * 80, f"\nSTAGE 1: logical candidates ({s1['combinations']} enumerated, capped={s1['capped']})")
for p in s1["pruned_candidates"]:
    print(f"  pass1 pruned: {compact(p)}")
for c in s1["candidates"]:
    print(f" {c.get('id')} {c.get('display_name', '')}")
    print_dag(c["dag"]) if "dag" in c else print(f"  error: {c.get('error')}")

s2 = d["stage2_physical_asap_optimization"]
print("=" * 80, "\nSTAGE 2: physical candidates")
for c in s2["candidates"]:
    print(f" {c['id']} from {c['from_logical']}  {c['display_name']}")
    print_dag(c["dag"])
for p in s2["pruned"]:
    print(f"  pruned {compact(p)}")
pruned = Counter((p["reason"], p["timing"]) for p in s2["pruned_candidates"])
for (reason, timing), n in sorted(pruned.items()):
    print(f"  pruned {n} {timing} candidates: {reason}")

s3 = d["stage3_plan_selection"]
print("=" * 80, "\nSTAGE 3: plan selection")
if "error" in s3:
    sys.exit(f"  error: {s3['error']}")
r = s3["search_report"]
print(f"  search {r['status']}: {r['milp_variables']} MILP variables, {r['solver_ms']} ms solving")
sel = s3["selected_dag"]
print(f"  selected Stage 2 candidates per node: {sel['candidate_of_node']}, outputs: {sel['node_of_output_reference']}")
for e in sel["edges"]:
    print(f"    node {e['producer']} -> node {e['consumer']} input {e['input']}")
print(f"  cost: {s3['cost']['cost_per_second']:.6g} cost/s")
for node, nc in s3["cost"]["per_node"].items():
    print(f"    #{node} {nc['operator']}: {nc['cost_per_second']:.6g}  {nc['detail']}")
for q, a in enumerate(s3["accepted_accuracy_per_query"]):
    print(f"  q{q} accuracy: {a['summary']}")
