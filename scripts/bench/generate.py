# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
"""A synthetic dbt project, to measure the proxy on projects of thousands of models.

usage: generate.py OUT MODELS COLUMNS [BIG_YAML_MB]

MODELS models in directories of 50, each directory with a properties file
whose entries have COLUMNS columns with descriptions and tests; sources,
macros (one dispatch), docs blocks, snapshots, seeds, singular tests, and a
package under dbt_packages.  BIG_YAML_MB, when given, adds one properties
file of about that size.
"""

import random
import shutil
import sys
from pathlib import Path

GROUP = 50
MACROS = [f"m_{k}" for k in range(60)]
OPTIONAL_ARGUMENT = 4


def write(path: Path, text: str) -> None:
    """Write TEXT at PATH, its directories made."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def model_sql(upstream: list[str], sources: list[tuple[str, str]]) -> str:
    """Give a model of about 40 lines: CTEs on its refs and sources, macro calls."""
    ctes = [
        f"up_{k} as (\n    select * from {{{{ ref('{ref}') }}}}\n)"
        for k, ref in enumerate(upstream)
    ]
    ctes += [
        f"src_{k} as (\n    select * from {{{{ source('{s}', '{t}') }}}}\n)"
        for k, (s, t) in enumerate(sources)
    ] or ["base as (\n    select 1 as id\n)"] * (not upstream)
    columns = [
        f"        {{{{ {MACROS[c % len(MACROS)]}('col_{c}') }}}} as col_{c},"
        for c in range(12)
    ]
    first = ctes[0].split(" ", 1)[0]
    lines = [
        "{{ config(materialized='table', tags=['synthetic']) }}",
        "",
        "with",
        ",\n\n".join(ctes) + ",",
        "",
        "renamed as (",
        "    select",
        *columns,
        "        {% for x in ['a', 'b', 'c'] %} col_{{ x }}, {% endfor %}",
        "        id",
        f"    from {first}",
        "    -- a comment with ref('nothing') in prose",
        ")",
        "",
        "select * from renamed",
    ]
    return "\n".join(lines) + "\n"


def column_yaml(c: int, model: str, refs: list[str]) -> list[str]:
    """Give the lines of column C of MODEL, its tests, a relationship to REFS[0]."""
    out = [
        f"      - name: col_{c}",
        f'        description: "Column {c} of {model}, a synthetic column."',
        "        data_tests:",
        "          - not_null",
    ]
    if c == 0:
        out.append("          - unique")
    if c % 5 == 1 and refs:
        out += [
            "          - relationships:",
            "              arguments:",
            f"                to: ref('{refs[0]}')",
            "                field: id",
        ]
    if c % 7 == 1:
        out += [
            "          - accepted_values:",
            "              arguments:",
            "                values: ['a', 'b', 'c', 'd']",
        ]
    return out


def model_yaml(models: list[str], columns: int, upstream: dict) -> str:
    """Give a properties file of MODELS, COLUMNS columns each."""
    out = ["version: 2", "", "models:"]
    for k, m in enumerate(models):
        out += [
            f"  - name: {m}",
            f"    description: '{{{{ doc(\"doc_{k % 100}\") }}}}'",
            "    config:",
            "      tags: ['synthetic']",
            "    columns:",
        ]
        for c in range(columns):
            out += column_yaml(c, m, upstream.get(m, []))
    return "\n".join(out) + "\n"


def sources_yaml(source: str, tables: list[str]) -> str:
    """Give a properties file declaring SOURCE with TABLES, 5 columns each."""
    out = ["version: 2", "", "sources:", f"  - name: {source}", "    tables:"]
    for t in tables:
        out += [f"      - name: {t}", "        columns:"]
        out += [f"          - name: c{c}\n            data_tests: [not_null]"
                for c in range(5)]
    return "\n".join(out) + "\n"


def fixed_files(out: Path, n_models: int) -> list[tuple[str, str]]:
    """Write the project file, sources, macros, docs; return the source tables."""
    write(out / "dbt_project.yml",
          "name: synth\nversion: '1.0'\nprofile: synth\n")
    tables = []
    for s in range(max(1, n_models // 150)):
        names = [f"t_{s}_{t}" for t in range(20)]
        tables += [(f"src_{s}", t) for t in names]
        write(out / f"models/staging/src_{s}/_sources.yml",
              sources_yaml(f"src_{s}", names))
    for m in MACROS:
        write(out / f"macros/{m}.sql",
              f"{{% macro {m}(c) %}}\n  coalesce({{{{ c }}}}, 0)\n{{% endmacro %}}\n")
    write(out / "macros/safe.sql",
          "{% macro safe(c) %}{{ return(adapter.dispatch('safe', 'synth')(c)) }}"
          "{% endmacro %}\n{% macro default__safe(c) %}{{ c }}{% endmacro %}\n")
    write(out / "macros/tests.sql",
          "{% test is_even(model, column_name) %}\nselect 1\n{% endtest %}\n")
    for d in range(100):
        write(out / f"models/docs/doc_{d}.md",
              f"{{% docs doc_{d} %}}\nThe doc {d}.\n{{% enddocs %}}\n")
    return tables


def models(out: Path, n_models: int, columns: int, tables: list) -> list[str]:
    """Write the models, a third in each layer, and their properties files."""
    names: list[str] = []
    upstream: dict[str, list[str]] = {}
    groups: dict[str, list[str]] = {}
    for i in range(n_models):
        layer = ["staging", "intermediate", "marts"][min(2, 3 * i // n_models)]
        name = f"{layer[:3]}_{i}"
        ups = random.sample(names, k=min(3, len(names))) if layer != "staging" else []
        srcs = [tables[i % len(tables)]] if layer == "staging" else []
        upstream[name] = ups
        group = f"models/{layer}/group_{i // GROUP}"
        groups.setdefault(group, []).append(name)
        write(out / group / f"{name}.sql", model_sql(ups, srcs))
        names.append(name)
    for group, held in groups.items():
        write(out / group / "_models.yml", model_yaml(held, columns, upstream))
    return names


def others(out: Path, names: list[str]) -> None:
    """Write snapshots, seeds, singular tests, and a package."""
    for k in range(20):
        write(out / f"snapshots/snap_{k}.sql",
              f"{{% snapshot snap_{k} %}}\nselect * from {{{{ ref('{names[k]}') }}}}\n"
              "{% endsnapshot %}\n")
        rows = "".join(f"{r},n{r}\n" for r in range(200))
        write(out / f"seeds/seed_{k}.csv", "id,name\n" + rows)
    for k in range(50):
        write(out / f"tests/assert_{k}.sql",
              f"select * from {{{{ ref('{names[-1 - k]}') }}}} where id < 0\n")
    package = out / "dbt_packages/utils"
    write(package / "dbt_project.yml", "name: utils\nversion: '1.0'\n")
    for k in range(150):
        write(package / f"macros/u_{k}.sql",
              f"{{% macro u_{k}(x) %}}{{{{ x }}}}{{% endmacro %}}\n")
    held = [f"util_{k}" for k in range(50)]
    for m in held:
        write(package / f"models/{m}.sql", "select {{ utils.u_1('a') }} as id\n")
    write(package / "models/_models.yml", model_yaml(held, 5, {}))


def big_yaml(out: Path, names: list[str], megabytes: float) -> None:
    """Write one properties file of about MEGABYTES, its models naming NAMES."""
    entries = []
    size = 0
    while size < megabytes * 1e6:
        k = len(entries)
        upstream = {f"big_{k}": [names[k % len(names)]]}
        entry = model_yaml([f"big_{k}"], 30, upstream).split("models:\n", 1)[1]
        entries.append(entry)
        size += len(entry)
    write(out / "models/big/_big.yml", "version: 2\n\nmodels:\n" + "".join(entries))


def main() -> None:
    """Generate the project the arguments describe, and report its size."""
    out, n_models, columns = Path(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
    random.seed(16)
    if out.exists():
        shutil.rmtree(out)
    tables = fixed_files(out, n_models)
    names = models(out, n_models, columns, tables)
    others(out, names)
    if len(sys.argv) == OPTIONAL_ARGUMENT + 1:
        big_yaml(out, names, float(sys.argv[OPTIONAL_ARGUMENT]))
    files = [p for p in out.rglob("*") if p.is_file()]
    total = sum(p.stat().st_size for p in files)
    yaml = sum(p.stat().st_size for p in files if p.suffix == ".yml")
    sys.stdout.write(f"{out}: {len(files)} files, {total / 1e6:.1f} MB, "
                     f"YAML {yaml / 1e6:.1f} MB\n")


main()
