#!/usr/bin/env python3
"""Generate the four Cargo composition crates from deep-module manifests.

Cargo resolves dependencies before build.rs runs, so composition manifests must
be refreshed before Cargo. Runtime services and SQL are inventory registrations
owned by each module; there is no second runtime module list here.

Base and Core modules sit one level deep (`apps/<layer>/<module>`). Domain and
Extension modules sit two levels deep inside a mounted container checkout
(`apps/<layer>/<container>/<module>`) that declares itself in
`bilibi.container.toml`. The committed composition is always the unmounted one,
so `--check` also refuses a staged or committed mounted composition or
`Cargo.lock`. `--check --staged` judges only the staged form, for the
pre-commit hook: a commit that removes a mounted composition already at HEAD
must be allowed. CI runs the full `--check`.

A company build mounts Domain and Extension containers at chosen revisions.
`--pin` records the Platform and container HEADs, the version each is at (its
`vX.Y.Z` tag, see docs/versions.md) and the resolved `Cargo.lock` under
`.local/composition/`; `apps/web/build.rs` compiles that
manifest into the program. `--restore` rebuilds exactly that composition
(containers at the pinned HEADs, the pinned `Cargo.lock`) so that
`cargo build --release --locked` is reproducible.

Every Core module is on demand (docs/architecture/decisions/25-core-modules-on-demand.md).
An unmounted Platform composes all of Core, so its own build and CI cover every
module. Once a Domain or Extension is mounted, `apps/core` composes only what the
host (`PLATFORM_MANIFESTS`) and the mounted modules name in their Cargo
`[dependencies]`, closed over the Core modules those need. A crate whose source
names a Core module outside its own declared closure is refused here, before
Cargo, so the gap shows as one plain line instead of a link or boot failure.
`--explain` prints the composed Core modules and who asked for each.
"""
import argparse
import hashlib
import re
import shutil
import subprocess
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parent.parent
LAYERS = ('base', 'core', 'domains', 'extensions')
CONTAINER_LAYERS = {'domains': 'domain', 'extensions': 'extension'}
CONTAINER_FILE = 'bilibi.container.toml'
RESERVED_IDS = {'base', 'core', 'web', 'src'}
SNAKE = re.compile(r'[a-z][a-z0-9]*(_[a-z0-9]+)*')
# Path packages the Platform itself owns besides discovered Base and Core modules.
PLATFORM_MANIFESTS = ('apps/web/Cargo.toml', 'apps/installer/Cargo.toml', 'fixtures/*/Cargo.toml')
PIN_DIR = Path('.local/composition')
PIN_FILES = ('Cargo.lock', 'manifest.txt')
# A release is a Git tag `vX.Y.Z` (docs/versions.md); the version is what follows the `v`.
VERSION_TAG = re.compile(r'v(\d+\.\d+\.\d+)')


class Refused(Exception):
    pass


def package_name(manifest):
    return tomllib.loads(manifest.read_text())['package']['name']


def snake_id(value, what, where):
    if not isinstance(value, str) or not SNAKE.fullmatch(value):
        raise Refused(f'{where}: {what} {value!r} must be snake_case ([a-z][a-z0-9_]*)')
    if value in RESERVED_IDS:
        raise Refused(f'{where}: {what} {value!r} is reserved ({", ".join(sorted(RESERVED_IDS))})')
    return value


def container_modules(root, layer):
    """Validate each mounted container of a Domain or Extension root; yield (path, crate)."""
    role = CONTAINER_LAYERS[layer]
    for folder in sorted((root / 'apps' / layer).iterdir()):
        if folder.name.startswith('.') or folder.name == 'src' or not (folder.is_dir() or folder.is_symlink()):
            continue
        where = f'apps/{layer}/{folder.name}'
        if folder.is_symlink():
            raise Refused(f'{where}: a container is mounted as a real checkout, not a symlink')
        for stray in ('Cargo.toml', 'Cargo.lock'):
            if (folder / stray).exists():
                raise Refused(f'{where}: a container has no root {stray}; only its modules are crates')
        descriptor = folder / CONTAINER_FILE
        if not descriptor.is_file():
            raise Refused(f'{where}: missing {CONTAINER_FILE}')
        try:
            declared = tomllib.loads(descriptor.read_text())
        except tomllib.TOMLDecodeError as error:
            raise Refused(f'{where}/{CONTAINER_FILE}: {error}') from None
        unknown = sorted(set(declared) - {'id', 'layer'})
        if unknown:
            raise Refused(f'{where}/{CONTAINER_FILE}: unknown keys {unknown}; expected id and layer')
        container = snake_id(declared.get('id'), 'container id', f'{where}/{CONTAINER_FILE}')
        if container != folder.name:
            raise Refused(f'{where}: {CONTAINER_FILE} declares id {container!r}; mount it at apps/{layer}/{container}')
        if declared.get('layer') != role:
            layer_folder = {v: k for k, v in CONTAINER_LAYERS.items()}.get(declared.get('layer'))
            hint = f'; mount it under apps/{layer_folder}' if layer_folder else ''
            raise Refused(f'{where}: {CONTAINER_FILE} declares layer {declared.get("layer")!r}, '
                          f'but apps/{layer} holds layer {role!r}{hint}')
        for manifest in sorted(folder.glob('*/Cargo.toml')):
            module = manifest.parent
            if module.name.startswith('.') or module.name == 'fixtures':
                continue
            if module.is_symlink():
                raise Refused(f'{where}/{module.name}: a module is a real folder, not a symlink')
            snake_id(module.name, 'module folder', f'{where}/{module.name}')
            name = package_name(manifest)
            expected = f'bilibi-{container}-{module.name}'.replace('_', '-')
            if name != expected:
                raise Refused(f'{where}/{module.name}/Cargo.toml: crate {name!r} must be named {expected!r}')
            yield f'{container}/{module.name}', name


def discovered(root):
    """Map each layer to its [(relative path, crate)] list."""
    result = {}
    names = set()
    containers = {}
    for layer in LAYERS:
        if layer in CONTAINER_LAYERS:
            modules = list(container_modules(root, layer))
        else:
            modules = [(manifest.parent.name, package_name(manifest))
                       for manifest in sorted((root / 'apps' / layer).glob('*/Cargo.toml'))]
        for path, name in modules:
            if name in names:
                raise Refused(f'Duplicate module crate: {name}')
            names.add(name)
            if layer in CONTAINER_LAYERS:
                container = path.split('/')[0]
                if containers.setdefault(container, layer) != layer:
                    raise Refused(f'Container {container!r} is mounted under both apps/domains and apps/extensions')
        result[layer] = modules
    return result


COMPOSITION_CRATE = 'bilibi-core'
# A Core module used in code: a service ID (`"core/<module>.<service>"`), a path
# through the composition crate, or the module's own crate. A bare
# `"core/<module>"` elsewhere is a record kind or slot owner, an optional
# coupling that simply stays empty when the module is absent.
CORE_REFERENCE = re.compile(r'"core/([a-z][a-z0-9_]*)\.|\bbilibi_core::([a-z][a-z0-9_]*)|\b(bilibi_[a-z0-9_]+)\b')
# A descriptor's `dependencies: &[...]` list, which boot also checks.
DESCRIPTOR_DEPENDENCIES = re.compile(r'\bdependencies\s*:\s*&\[([^\]]*)\]')
ADR = 'docs/architecture/decisions/25-core-modules-on-demand.md'


def dependencies(manifest):
    """Package names in a manifest's normal [dependencies], including target-specific ones."""
    data = tomllib.loads(manifest.read_text())
    tables = [data.get('dependencies', {})]
    tables += [target.get('dependencies', {}) for target in data.get('target', {}).values()]
    return {value.get('package', key) if isinstance(value, dict) else key
            for table in tables for key, value in table.items()}


def core_graph(root, modules):
    """Map each Core crate to (module ID, the Core crates it depends on)."""
    crates = {name: path for path, name in modules['core']}
    return {name: (f'core/{path}', dependencies(root / 'apps/core' / path / 'Cargo.toml') & set(crates))
            for name, path in crates.items()}


def core_closure(graph, start):
    seen, pending = set(), [name for name in start if name in graph]
    while pending:
        name = pending.pop()
        if name not in seen:
            seen.add(name)
            pending.extend(graph[name][1] - seen)
    return seen


def host_manifests(root):
    return [manifest for pattern in PLATFORM_MANIFESTS for manifest in sorted(root.glob(pattern))]


def mounted_manifests(root, modules):
    return [(f'apps/{layer}/{path}', root / 'apps' / layer / path / 'Cargo.toml')
            for layer in CONTAINER_LAYERS for path, _ in modules[layer]]


def core_requests(root, modules):
    """Map each composing crate's label to the Core crates it names directly; None when nothing is mounted."""
    mounted = mounted_manifests(root, modules)
    if not mounted:
        return None
    graph = core_graph(root, modules)
    requests = {str(manifest.parent.relative_to(root)): dependencies(manifest) & set(graph)
                for manifest in host_manifests(root)}
    requests.update({label: dependencies(manifest) & set(graph) for label, manifest in mounted})
    return requests


def composed_core(root, modules):
    """The Core (path, crate) pairs this composition links: all of them unmounted, else the declared closure."""
    requests = core_requests(root, modules)
    if requests is None:
        return modules['core']
    wanted = core_closure(core_graph(root, modules), set().union(*requests.values()))
    return [(path, name) for path, name in modules['core'] if name in wanted]


def core_uses(folder):
    """Yield (file, line number, kind, name) for each Core module named in Rust sources under folder."""
    for source in sorted(folder.rglob('*.rs')):
        text = source.read_text(errors='replace')
        for match in DESCRIPTOR_DEPENDENCIES.finditer(text):
            number = text.count('\n', 0, match.start()) + 1
            for module in re.findall(r'"core/([a-z][a-z0-9_]*)"', match.group(1)):
                yield source, number, 'id', module
        for number, line in enumerate(text.splitlines(), 1):
            if line.lstrip().startswith('//'):
                continue
            for match in CORE_REFERENCE.finditer(line):
                service, through_core, ident = match.groups()
                yield source, number, *(('id', service) if service else ('id', through_core) if through_core
                                        else ('crate', ident))


def check_core_use(root, modules):
    """Refuse a crate whose src/ names a Core module it does not declare, directly or through a declared one."""
    graph = core_graph(root, modules)
    by_id = {module_id: name for name, (module_id, _) in graph.items()}
    by_ident = {name.replace('-', '_'): name for name in graph}
    # Base may not depend on Core at all (the registry refuses it), so it is not scanned.
    crates = [(f'apps/core/{path}', root / 'apps/core' / path / 'Cargo.toml', {name}) for path, name in modules['core']]
    crates += [(str(manifest.parent.relative_to(root)), manifest, set()) for manifest in host_manifests(root)]
    for label, manifest in mounted_manifests(root, modules):
        if COMPOSITION_CRATE in dependencies(manifest):
            raise Refused(f'{label}/Cargo.toml depends on {COMPOSITION_CRATE}, which holds only what the application '
                          'composes; depend on each Core module crate you use instead, for example '
                          f'bilibi-work-calendar.workspace = true (see {ADR})')
        crates.append((label, manifest, set()))
    for label, manifest, own in crates:
        allowed = core_closure(graph, own | (dependencies(manifest) & set(graph)))
        for source, number, kind, value in core_uses(manifest.parent / 'src'):
            name = by_id.get(f'core/{value}') if kind == 'id' else by_ident.get(value)
            if name and name not in allowed:
                used = graph[name][0]
                raise Refused(f'{source.relative_to(root)}:{number} uses {used}, but {label}/Cargo.toml does not declare it. '
                              f'Add {name}.workspace = true to its [dependencies] (and "{used}" to its descriptor '
                              f'dependencies when it looks up {used} services); an application links a Core module only '
                              f'when something that uses it declares it (see {ADR})')


def check_lints(root, modules):
    """Refuse a Platform crate that does not inherit the workspace lints, which deny unsafe code."""
    manifests = [root / 'apps' / layer / path / 'Cargo.toml' for layer in ('base', 'core') for path, _ in modules[layer]]
    for manifest in manifests + host_manifests(root):
        if tomllib.loads(manifest.read_text()).get('lints', {}).get('workspace') is not True:
            raise Refused(f'{manifest.relative_to(root)} does not inherit the workspace lints. Add a [lints] table '
                          'with workspace = true, so unsafe code is refused unless a documented '
                          '#[allow(unsafe_code, reason = "...")] covers it')


def explain(root, modules):
    requests = core_requests(root, modules)
    if requests is None:
        print("Core: every module (nothing is mounted, so this is the Platform's own build)")
        return
    graph = core_graph(root, modules)
    composed = {name for _, name in composed_core(root, modules)}
    print(f'Core modules in this composition ({len(composed)} of {len(graph)}), each with who declares it:')
    for path, name in modules['core']:
        module_id = graph[name][0]
        if name not in composed:
            print(f'  -  {module_id}: nothing declares it')
            continue
        askers = sorted(label for label, wanted in requests.items() if name in wanted)
        askers += sorted(graph[other][0] for other in composed if name in graph[other][1])
        print(f'  +  {module_id}: {", ".join(askers)}')


def composition(layer, modules):
    text = (f'# Generated by scripts/discover-modules.py; edit deep-module manifests.\n'
            f'[package]\nname = "bilibi-{layer}"\nversion.workspace = true\n'
            'edition.workspace = true\nrust-version.workspace = true\nlicense.workspace = true\n\n[dependencies]\n')
    text += ''.join(f'{name} = {{ path = "{path}" }}\n' for path, name in modules)
    text += '\n[lints]\nworkspace = true\n'
    code = '// Generated composition links. Runtime inventory owns discovery.\n'
    # Rustfmt sorts imports by Rust crate identifier, not container/module path.
    # Keep discovery order for the manifest and MODULES table below.
    imports = sorted(modules, key=lambda module: module[1].replace('-', '_'))
    code += ''.join(f'pub use {name.replace("-", "_")} as {path.replace("/", "_")};\n' for path, name in imports)
    if layer in CONTAINER_LAYERS:
        # A mounted module's ID is its <container>/<module> path; boot refuses descriptors that differ.
        code += '/// Mounted (module ID, crate) pairs, checked against registered descriptors at boot.\n#[rustfmt::skip]\n'
        code += 'pub const MODULES: &[(&str, &str)] = &[' + ', '.join(f'("{path}", "{name}")' for path, name in modules) + '];\n'
    code += 'pub fn link() {}\n'
    return {f'apps/{layer}/Cargo.toml': text, f'apps/{layer}/src/lib.rs': code}


def outputs(root, modules):
    check_core_use(root, modules)
    result = {}
    for layer in LAYERS:
        members = composed_core(root, modules) if layer == 'core' else modules[layer]
        result.update({root / path: text for path, text in composition(layer, members).items()})
    return result


def platform_crates(root, modules):
    names = {f'bilibi-{layer}' for layer in LAYERS}
    names.update(name for layer in ('base', 'core') for _, name in modules[layer])
    for pattern in PLATFORM_MANIFESTS:
        names.update(package_name(manifest) for manifest in root.glob(pattern))
    return names


def git_versions(root, path, staged_only):
    """Yield (label, text) for the committed and staged forms of a tracked file."""
    forms = (('committed', f'HEAD:{path}'), ('staged', f':{path}'))
    for label, spec in forms[1:] if staged_only else forms:
        shown = subprocess.run(['git', '-C', str(root), 'show', spec], capture_output=True, text=True)
        if shown.returncode == 0:
            yield label, shown.stdout


def check_unmounted_commit(root, modules, staged_only):
    """Refuse a staged or committed composition that names mounted Domain or Extension crates."""
    inside = subprocess.run(['git', '-C', str(root), 'rev-parse', '--show-toplevel'],
                            capture_output=True, text=True)
    if inside.returncode != 0 or Path(inside.stdout.strip()).resolve() != root.resolve():
        raise Refused('--check needs the Platform Git checkout to read its staged and committed composition')
    advice = ('Never commit composition files or Cargo.lock from a mounted checkout; '
              'unmount, run python3 scripts/discover-modules.py, and let Cargo prune Cargo.lock (see apps/domains/AGENTS.md)')
    for layer in CONTAINER_LAYERS:
        for path, text in composition(layer, []).items():
            for label, shown in git_versions(root, path, staged_only):
                if shown != text:
                    raise Refused(f'The {label} {path} is a mounted composition. {advice}')
    allowed = platform_crates(root, modules)
    for label, shown in git_versions(root, 'Cargo.lock', staged_only):
        lock = tomllib.loads(shown)
        foreign = sorted(p['name'] for p in lock.get('package', []) if 'source' not in p and p['name'] not in allowed)
        if foreign:
            raise Refused(f'The {label} Cargo.lock names path packages outside the Platform: {", ".join(foreign)}. {advice}')


def git(path, *args):
    shown = subprocess.run(['git', '-C', str(path), *args], capture_output=True, text=True)
    if shown.returncode != 0:
        raise Refused(f'git {" ".join(args)} failed in {path}: {shown.stderr.strip() or "not a Git checkout"}')
    return shown.stdout


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def mounted_containers(root, modules):
    """Map `<layer>/<container>` to its folder for every container that mounts modules."""
    return {f'{layer}/{path.split("/")[0]}': root / 'apps' / layer / path.split('/')[0]
            for layer in CONTAINER_LAYERS for path, _ in modules[layer]}


def checkout_head(label, folder):
    """HEAD of a clean, independent checkout; a company build records nothing less."""
    if Path(git(folder, 'rev-parse', '--show-toplevel').strip()).resolve() != folder.resolve():
        raise Refused(f'{label} is not its own Git checkout; mount it with git clone')
    if git(folder, 'status', '--porcelain').strip():
        raise Refused(f'{label} has uncommitted or untracked files; commit them in its repository first')
    return git(folder, 'rev-parse', 'HEAD').strip()


def version_at_head(label, folder):
    """The version a `vX.Y.Z` tag at HEAD names, or None when HEAD carries no version tag."""
    tags = git(folder, 'tag', '--points-at', 'HEAD').split()
    versions = sorted({match.group(1) for tag in tags if (match := VERSION_TAG.fullmatch(tag))})
    if len(versions) > 1:
        raise Refused(f'{label} carries several version tags at HEAD ({", ".join("v" + v for v in versions)}); keep one')
    return versions[0] if versions else None


def workspace_version(root):
    return tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['version']


def require_resolved_lock(root):
    resolved = subprocess.run(['cargo', 'metadata', '--locked', '--format-version', '1'],
                              cwd=root, capture_output=True, text=True)
    if resolved.returncode != 0:
        raise Refused('Cargo.lock does not resolve the mounted composition; run any Cargo command '
                      f'(for example cargo metadata) without --locked first. {resolved.stderr.strip()}')


def pin(root, modules, files):
    """Record the Platform and container HEADs and the resolved Cargo.lock under .local/composition."""
    changed = [line[3:] for line in git(root, 'status', '--porcelain', '--untracked-files=no').splitlines()]
    composition_files = {str(path.relative_to(root)) for path in files} | {'Cargo.lock'}
    unexpected = sorted(set(changed) - composition_files)
    if unexpected:
        raise Refused(f'Platform has uncommitted changes outside the composition: {", ".join(unexpected)}')
    mounted = mounted_containers(root, modules)
    heads = {name: checkout_head(f'apps/{name}', folder) for name, folder in sorted(mounted.items())}
    versions = {name: version_at_head(f'apps/{name}', folder) for name, folder in sorted(mounted.items())}
    require_resolved_lock(root)
    lock = root / 'Cargo.lock'
    platform = git(root, 'rev-parse', 'HEAD').strip()
    platform_version = workspace_version(root)
    tagged = version_at_head('The Platform', root)
    if tagged is not None and tagged != platform_version:
        raise Refused(f'The Platform is tagged v{tagged} but Cargo.toml says version {platform_version}; '
                      'a release tag names the Cargo.toml version (docs/versions.md)')
    manifest = ('# Written by scripts/discover-modules.py --pin; read by --restore and apps/web/build.rs.\n'
                f'platform = "{platform}"\n'
                f'platform_version = "{platform_version}"\n'
                + (f'platform_tag = "v{tagged}"\n' if tagged else '')
                + f'cargo_lock_sha256 = "{sha256(lock)}"\n'
                + '\n[containers]\n')
    for name, head in heads.items():
        manifest += f'\n[containers."{name}"]\nhead = "{head}"\n'
        if versions[name]:
            manifest += f'version = "{versions[name]}"\n'
    target = root / PIN_DIR
    target.mkdir(parents=True, exist_ok=True)
    # Written only when changed: the executable host's build script watches
    # this folder, so an untouched pin keeps the compiled crates fresh.
    if not (target / 'Cargo.lock').is_file() or (target / 'Cargo.lock').read_bytes() != lock.read_bytes():
        shutil.copyfile(lock, target / 'Cargo.lock')
    if not (target / 'manifest.txt').is_file() or (target / 'manifest.txt').read_text() != manifest:
        (target / 'manifest.txt').write_text(manifest)
    described = ', '.join(f'{name} {"v" + versions[name] if versions[name] else head[:12] + " (untagged)"}'
                          for name, head in heads.items())
    platform_label = f'v{tagged}' if tagged else f'{platform[:12]} (unreleased {platform_version})'
    print(f'pinned {len(heads)} container(s) at Platform {platform_label} in {PIN_DIR}: {described}')


def restore(root, modules, files):
    """Rebuild the pinned composition: refuse any drift, then put the pinned Cargo.lock back."""
    target = root / PIN_DIR
    missing = [name for name in PIN_FILES if not (target / name).is_file()]
    if missing:
        raise Refused(f'{PIN_DIR} lacks {", ".join(missing)}; run --pin on the machine that built the release')
    try:
        manifest = tomllib.loads((target / 'manifest.txt').read_text())
        platform, digest, pinned = manifest['platform'], manifest['cargo_lock_sha256'], manifest['containers']
    except (tomllib.TOMLDecodeError, KeyError) as error:
        raise Refused(f'{PIN_DIR}/manifest.txt is unreadable ({error!r}); run --pin again') from None
    head = git(root, 'rev-parse', 'HEAD').strip()
    if head != platform:
        raise Refused(f'Platform is at {head[:12]} but the pin was made at {platform[:12]}; check out the pinned revision')
    if sha256(target / 'Cargo.lock') != digest:
        raise Refused(f'{PIN_DIR}/Cargo.lock does not match the pinned SHA-256; run --pin again')
    mounted = mounted_containers(root, modules)
    for name in sorted(set(pinned) - set(mounted)):
        raise Refused(f'apps/{name} is pinned but not mounted; clone it and check out {pinned[name]["head"][:12]}')
    for name in sorted(set(mounted) - set(pinned)):
        raise Refused(f'apps/{name} is mounted but not in the pin; unmount it or run --pin again')
    for name, folder in sorted(mounted.items()):
        actual = checkout_head(f'apps/{name}', folder)
        if actual != pinned[name]['head']:
            raise Refused(f'apps/{name} is at {actual[:12]} but the pin was made at {pinned[name]["head"][:12]}; '
                          f'run git -C apps/{name} checkout {pinned[name]["head"]}')
    write_outputs(files)
    shutil.copyfile(target / 'Cargo.lock', root / 'Cargo.lock')
    print(f'restored {len(pinned)} container(s) and Cargo.lock; build with cargo build --release --locked')


def write_outputs(files):
    for path, text in files.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        if not path.exists() or path.read_text() != text:
            path.write_text(text)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--check', action='store_true',
                      help='refuse stale composition files and a staged or committed mounted composition')
    mode.add_argument('--pin', action='store_true',
                      help='record the mounted container HEADs and the resolved Cargo.lock in .local/composition')
    mode.add_argument('--restore', action='store_true',
                      help='rebuild the pinned composition, refusing any container or Platform drift')
    mode.add_argument('--explain', action='store_true',
                      help='print the Core modules this composition links and who declares each')
    parser.add_argument('--staged', action='store_true',
                        help='with --check, judge only the staged composition and Cargo.lock, not HEAD')
    args = parser.parse_args()
    if args.staged and not args.check:
        parser.error('--staged only applies to --check')
    try:
        modules = discovered(ROOT)
        check_lints(ROOT, modules)
        files = outputs(ROOT, modules)
        if args.explain:
            explain(ROOT, modules)
        elif args.restore:
            restore(ROOT, modules, files)
        elif args.pin:
            write_outputs(files)
            pin(ROOT, modules, files)
        elif args.check:
            for path, text in files.items():
                if not path.exists() or path.read_text() != text:
                    raise Refused(f'Composition is stale: {path}. Run python3 scripts/discover-modules.py')
            check_unmounted_commit(ROOT, modules, args.staged)
        else:
            write_outputs(files)
    except Refused as refusal:
        raise SystemExit(f'discover-modules: {refusal}') from None


if __name__ == '__main__':
    main()
