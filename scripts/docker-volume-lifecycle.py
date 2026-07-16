#!/usr/bin/env python3
"""以精确名称、完整标签和前后集合门禁治理 GetLatestRepo Docker 卷。"""

from __future__ import annotations

import argparse
import json
import os
import secrets
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, NoReturn


# 从脚本位置推导项目根目录，避免调用者切换工作目录后读错合同文件。
PROJECT_ROOT = Path(__file__).resolve().parents[1]
# 运行卷真相只来自版本化 manifest，不从 Compose 项目名反推物理卷名。
DEFAULT_MANIFEST = PROJECT_ROOT / "docker" / "volumes.manifest.json"
# 普通测试和持久化测试共用已构建的后端镜像，确保测试真实覆盖镜像的挂载行为。
DEFAULT_IMAGE = "pro-get-latest-repo-backend:latest"
# 正常测试 TTL 必须明显长于测试时长，避免并行任务误删彼此的资源。
DEFAULT_TTL_SECONDS = 3600


class ContractError(RuntimeError):
    """表示生命周期合同被违反；调用方应失败关闭，不能猜测后继续删除。"""


class LifecycleInterrupted(RuntimeError):
    """表示测试收到 INT 或 TERM；finally 仍负责精确清理当前 run-id。"""

    def __init__(self, signum: int) -> None:
        super().__init__(f"收到信号 {signum}")
        self.signum = signum


@dataclass(frozen=True)
class Contract:
    """保存经过校验的 manifest，后续逻辑只读取这一份结构化真相。"""

    project: str
    runtime_namespace: str
    test_namespace: str
    volumes: tuple[dict[str, str], ...]

    @property
    def approved_names(self) -> set[str]:
        """返回运行卷精确物理名称集合，用作任何测试删除前的最高优先级否决项。"""

        return {volume["name"] for volume in self.volumes}

    @property
    def protected_label(self) -> str:
        """返回运行卷保护标签键；值为 true 时测试清理器必须拒绝删除。"""

        return f"{self.runtime_namespace}.protected"


def fail(message: str) -> NoReturn:
    """统一抛出中文合同错误，让 CLI 和 CI 日志保持同一语义。"""

    raise ContractError(message)


def load_contract(path: Path) -> Contract:
    """读取并严格校验版本化 manifest，拒绝空清单、重复名称和未知数据类别。"""

    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"无法读取卷 manifest {path}：{error}")

    if raw.get("schema_version") != 1:
        fail("卷 manifest.schema_version 必须为 1")

    project = raw.get("project")
    runtime_namespace = raw.get("runtime_label_namespace")
    test_namespace = raw.get("test_label_namespace")
    volumes = raw.get("volumes")
    if not all(isinstance(value, str) and value for value in (project, runtime_namespace, test_namespace)):
        fail("卷 manifest 的 project 与两个 label namespace 必须是非空字符串")
    if not isinstance(volumes, list) or not volumes:
        fail("卷 manifest.volumes 必须是非空数组")

    normalized: list[dict[str, str]] = []
    for index, volume in enumerate(volumes):
        if not isinstance(volume, dict):
            fail(f"卷 manifest.volumes[{index}] 必须是对象")
        required = ("compose_key", "name", "driver", "purpose", "data_class")
        if not all(isinstance(volume.get(key), str) and volume[key] for key in required):
            fail(f"卷 manifest.volumes[{index}] 缺少非空字段：{', '.join(required)}")
        if volume["data_class"] != "runtime-truth":
            fail(f"运行卷 {volume['name']} 的 data_class 必须是 runtime-truth")
        normalized.append({key: volume[key] for key in required})

    compose_keys = [volume["compose_key"] for volume in normalized]
    physical_names = [volume["name"] for volume in normalized]
    if len(set(compose_keys)) != len(compose_keys):
        fail("卷 manifest 存在重复 compose_key")
    if len(set(physical_names)) != len(physical_names):
        fail("卷 manifest 存在重复物理卷名")

    return Contract(
        project=project,
        runtime_namespace=runtime_namespace,
        test_namespace=test_namespace,
        volumes=tuple(normalized),
    )


def run(
    arguments: list[str],
    *,
    check: bool = True,
    capture: bool = True,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """执行外部命令；默认捕获文本输出并把失败转换为含 stderr 的合同错误。"""

    try:
        return subprocess.run(
            arguments,
            check=check,
            capture_output=capture,
            text=True,
            env=env,
        )
    except FileNotFoundError:
        fail(f"找不到命令：{arguments[0]}")
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip()
        fail(f"命令失败（{' '.join(arguments)}）：{detail or f'退出码 {error.returncode}'}")


def docker_json(arguments: list[str]) -> Any:
    """执行返回 JSON 的 Docker 命令，并拒绝空输出或非法 JSON。"""

    completed = run(["docker", *arguments])
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        fail(f"Docker 返回非法 JSON（docker {' '.join(arguments)}）：{error}")


def resource_exists(kind: str, name: str) -> bool:
    """使用 inspect 精确判断资源是否存在，不使用名称前缀或模糊匹配。"""

    completed = run(["docker", kind, "inspect", name], check=False)
    return completed.returncode == 0


def volume_snapshot() -> set[str]:
    """读取 Docker daemon 的完整卷名称集合，作为测试前后不可变门禁。"""

    completed = run(["docker", "volume", "ls", "--format", "{{.Name}}"])
    return {line for line in completed.stdout.splitlines() if line}


def compare_volume_sets(before: set[str], after: set[str]) -> None:
    """比较完整集合；只报告差异，绝不替调用者删除未知卷。"""

    added = sorted(after - before)
    removed = sorted(before - after)
    if not added and not removed:
        return
    lines = ["Docker volume 全局集合发生变化"]
    lines.extend(f"新增：{name}" for name in added)
    lines.extend(f"减少：{name}" for name in removed)
    fail("；".join(lines))


def guard_command(arguments: list[str]) -> int:
    """执行任意命令并轮询比较全局卷集合；只报告差异，不自动删除未知卷。"""

    command = list(arguments)
    if command and command[0] == "--":
        command.pop(0)
    if not command:
        fail("guard 必须通过 -- 指定被守卫命令")

    before = volume_snapshot()
    completed = run(command, check=False, capture=False)
    attempts = int(os.environ.get("DOCKER_VOLUME_GUARD_ATTEMPTS", "20"))
    interval = float(os.environ.get("DOCKER_VOLUME_GUARD_INTERVAL_SECONDS", "1"))
    if attempts <= 0 or interval < 0:
        fail("Docker volume guard 轮询参数非法")

    last_error: ContractError | None = None
    for attempt in range(attempts):
        try:
            compare_volume_sets(before, volume_snapshot())
            last_error = None
            break
        except ContractError as error:
            last_error = error
            if attempt + 1 < attempts:
                time.sleep(interval)
    if last_error is not None:
        raise last_error

    if completed.returncode == 0:
        print("✓ Docker volume 集合与命令执行前完全一致")
    return completed.returncode


def runtime_labels(contract: Contract, volume: dict[str, str]) -> dict[str, str]:
    """为新建运行卷生成稳定标签；已有卷标签不可变，因此供给器不会重建旧卷。"""

    namespace = contract.runtime_namespace
    return {
        f"{namespace}.project": contract.project,
        f"{namespace}.class": volume["data_class"],
        f"{namespace}.protected": "true",
        f"{namespace}.manifest-version": "1",
    }


def test_labels(
    contract: Contract,
    *,
    scope: str,
    run_id: str,
    created_at: int,
    expires_at: int,
) -> dict[str, str]:
    """为测试容器、网络和临时卷生成完全一致的五元标签合同。"""

    namespace = contract.test_namespace
    return {
        f"{namespace}.project": contract.project,
        f"{namespace}.scope": scope,
        f"{namespace}.run-id": run_id,
        f"{namespace}.created-at": str(created_at),
        f"{namespace}.expires-at": str(expires_at),
    }


def label_arguments(labels: dict[str, str]) -> list[str]:
    """把标签映射转换为 Docker CLI 的重复 --label 参数。"""

    arguments: list[str] = []
    for key, value in labels.items():
        arguments.extend(["--label", f"{key}={value}"])
    return arguments


def provision(contract: Contract) -> None:
    """幂等创建缺失运行卷；已有卷只校验，绝不为补标签而删除或重建。"""

    for volume in contract.volumes:
        name = volume["name"]
        expected_driver = volume["driver"]
        if not resource_exists("volume", name):
            run(
                [
                    "docker",
                    "volume",
                    "create",
                    "--driver",
                    expected_driver,
                    *label_arguments(runtime_labels(contract, volume)),
                    name,
                ],
                capture=False,
            )
            print(f"✓ 已创建运行卷：{name}")
            continue

        inspected = docker_json(["volume", "inspect", name])[0]
        if inspected.get("Driver") != expected_driver:
            fail(f"现有运行卷 {name} 的 driver={inspected.get('Driver')}，期望 {expected_driver}")

        labels = inspected.get("Labels") or {}
        expected_labels = runtime_labels(contract, volume)
        missing = [
            f"{key}={value}"
            for key, value in expected_labels.items()
            if labels.get(key) != value
        ]
        if missing:
            print(
                f"⚠ 现有运行卷 {name} 缺少新合同标签：{', '.join(missing)}；"
                "Docker 卷标签不可变，已保留原卷并继续依赖 manifest 精确名称保护。",
                file=sys.stderr,
            )
        else:
            print(f"✓ 运行卷已存在且标签匹配：{name}")


def render_compose(path: Path, environment: dict[str, str]) -> dict[str, Any]:
    """通过 Docker Compose 官方渲染器读取最终模型，覆盖变量和 override 语义。"""

    completed = run(
        ["docker", "compose", "-f", str(path), "config", "--format", "json"],
        env=environment,
    )
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        fail(f"Compose 模型不是合法 JSON（{path}）：{error}")


def audit_compose(
    contract: Contract,
    runtime_compose: Path,
    test_compose: Path,
) -> None:
    """审计运行栈只用 external 白名单卷，普通测试栈完全不声明 volume。"""

    runtime_environment = os.environ.copy()
    runtime_environment.setdefault("GETLATESTREPO_SCAN_ROOT", str(PROJECT_ROOT))
    runtime_environment.setdefault("GETLATESTREPO_SSH_DIR", str(PROJECT_ROOT))
    runtime = render_compose(runtime_compose, runtime_environment)

    approved = {volume["compose_key"]: volume["name"] for volume in contract.volumes}
    configured = runtime.get("volumes") or {}
    if set(configured) != set(approved):
        fail("运行 Compose 的卷键必须与 manifest 完全一致")
    for key, name in approved.items():
        definition = configured[key]
        if definition.get("external") is not True or definition.get("name") != name:
            fail(f"运行卷 {key} 必须 external=true 且 name={name}")

    for service_name, service in (runtime.get("services") or {}).items():
        for mount in service.get("volumes") or []:
            if mount.get("type") != "volume":
                continue
            source = mount.get("source")
            if not source:
                fail(f"运行服务 {service_name} 使用匿名卷挂载 {mount.get('target')}")
            if source not in approved:
                fail(f"运行服务 {service_name} 使用未批准卷 {source}")

    test_environment = os.environ.copy()
    now = int(time.time())
    test_environment.update(
        {
            "GETLATESTREPO_TEST_PROJECT": "glr-contract-audit",
            "GETLATESTREPO_TEST_RUN_ID": "contract-audit",
            "GETLATESTREPO_TEST_CREATED_AT": str(now),
            "GETLATESTREPO_TEST_EXPIRES_AT": str(now + DEFAULT_TTL_SECONDS),
        }
    )
    test_model = render_compose(test_compose, test_environment)
    if test_model.get("volumes"):
        fail("普通测试 Compose 禁止声明 top-level volumes")

    for service_name, service in (test_model.get("services") or {}).items():
        for mount in service.get("volumes") or []:
            if mount.get("type") == "volume":
                fail(f"普通测试服务 {service_name} 使用 volume 挂载")
        tmpfs_targets = {
            str(entry).split(":", 1)[0]
            for entry in service.get("tmpfs") or []
        }
        if "/data" not in tmpfs_targets:
            fail(f"普通测试服务 {service_name} 必须用 tmpfs 覆盖 /data")

    print(f"✓ Docker 卷静态合同通过：{len(approved)} 个 external 运行卷，普通测试零 volume")


def inspect_container(container: str) -> dict[str, Any]:
    """读取单个容器完整 inspect 结果，保证后续挂载判断基于真实运行模型。"""

    inspected = docker_json(["container", "inspect", container])
    if len(inspected) != 1:
        fail(f"容器 inspect 返回数量异常：{container}")
    return inspected[0]


def assert_no_volume_mounts(container: str, required_tmpfs: tuple[str, ...]) -> None:
    """拒绝所有 named/anonymous volume，并确认关键数据目录实际挂载为 tmpfs。"""

    inspected = inspect_container(container)
    mounts = inspected.get("Mounts") or []
    volume_mounts = [mount for mount in mounts if mount.get("Type") == "volume"]
    if volume_mounts:
        descriptions = [
            f"{mount.get('Name') or '<anonymous>'}->{mount.get('Destination')}"
            for mount in volume_mounts
        ]
        fail(f"普通测试容器 {container} 出现 volume：{', '.join(descriptions)}")

    tmpfs_targets = {
        mount.get("Destination")
        for mount in mounts
        if mount.get("Type") == "tmpfs"
    }
    tmpfs_targets.update((inspected.get("HostConfig") or {}).get("Tmpfs") or {})
    missing = sorted(set(required_tmpfs) - tmpfs_targets)
    if missing:
        fail(f"普通测试容器 {container} 缺少 tmpfs：{', '.join(missing)}")

    print(f"✓ 容器真实挂载通过：{container} 无 volume，tmpfs={','.join(required_tmpfs)}")


def inspect_labels(kind: str, identifier: str) -> tuple[str, dict[str, str], dict[str, Any]]:
    """统一读取容器、网络或卷的名称、标签和原始 inspect 数据。"""

    inspected = docker_json([kind, "inspect", identifier])[0]
    if kind == "container":
        name = str(inspected.get("Name") or "").lstrip("/")
        labels = (inspected.get("Config") or {}).get("Labels") or {}
    else:
        name = str(inspected.get("Name") or identifier)
        labels = inspected.get("Labels") or {}
    return name, labels, inspected


def owned_resource_ids(
    contract: Contract,
    kind: str,
    scope: str,
    run_id: str | None = None,
) -> list[str]:
    """先按稳定 project 标签枚举，再检查 scope/run-id，避免漏掉部分标签资源。"""

    namespace = contract.test_namespace
    filters = [
        "--filter",
        f"label={namespace}.project={contract.project}",
    ]

    if kind == "container":
        command = ["docker", "container", "ls", "-aq", *filters]
    elif kind == "network":
        command = ["docker", "network", "ls", "-q", *filters]
    elif kind == "volume":
        command = ["docker", "volume", "ls", "-q", *filters]
    else:
        fail(f"不支持的 Docker 资源类型：{kind}")

    completed = run(command)
    selected: list[str] = []
    for identifier in (line for line in completed.stdout.splitlines() if line):
        _, labels, _ = inspect_labels(kind, identifier)
        resource_scope = labels.get(f"{namespace}.scope")
        if resource_scope is None:
            fail(f"{kind} {identifier} 有 project 标签但缺少 scope 标签")
        if resource_scope != scope:
            continue
        if run_id is not None:
            resource_run_id = labels.get(f"{namespace}.run-id")
            if resource_run_id is None:
                fail(f"{kind} {identifier} 有 project/scope 标签但缺少 run-id 标签")
            if resource_run_id != run_id:
                continue
        selected.append(identifier)
    return selected


def validate_test_labels(
    contract: Contract,
    labels: dict[str, str],
    *,
    scope: str,
    now: int,
    expected_run_id: str | None = None,
) -> tuple[str, int, int]:
    """严格校验五元标签，并返回 run-id、创建时间和过期时间。"""

    namespace = contract.test_namespace
    required_keys = {
        "project": f"{namespace}.project",
        "scope": f"{namespace}.scope",
        "run-id": f"{namespace}.run-id",
        "created-at": f"{namespace}.created-at",
        "expires-at": f"{namespace}.expires-at",
    }
    missing = [key for key, label_key in required_keys.items() if not labels.get(label_key)]
    if missing:
        fail(f"测试资源缺少标签：{', '.join(missing)}")
    if labels[required_keys["project"]] != contract.project:
        fail("测试资源 project 标签不匹配")
    if labels[required_keys["scope"]] != scope:
        fail("测试资源 scope 标签不匹配")

    run_id = labels[required_keys["run-id"]]
    if expected_run_id is not None and run_id != expected_run_id:
        fail(f"测试资源 run-id={run_id}，期望 {expected_run_id}")
    try:
        created_at = int(labels[required_keys["created-at"]])
        expires_at = int(labels[required_keys["expires-at"]])
    except ValueError:
        fail("测试资源 created-at/expires-at 必须是 Unix 秒整数")
    if created_at <= 0 or expires_at <= created_at:
        fail("测试资源时间标签顺序非法")
    if now < 0:
        fail("当前时间非法")
    return run_id, created_at, expires_at


def assert_volume_deletable(
    contract: Contract,
    name: str,
    labels: dict[str, str],
) -> None:
    """在删除测试卷前执行运行卷精确名称与 protected 标签双重否决。"""

    if name in contract.approved_names:
        fail(f"拒绝删除 manifest 运行卷：{name}")
    if labels.get(contract.protected_label, "").lower() == "true":
        fail(f"拒绝删除带 protected=true 的卷：{name}")
    references = run(
        ["docker", "container", "ls", "-aq", "--filter", f"volume={name}"]
    ).stdout.splitlines()
    if references:
        fail(f"拒绝删除仍被容器引用的卷 {name}：{', '.join(references)}")


def cleanup_run(contract: Contract, scope: str, run_id: str) -> None:
    """只清理当前精确 run-id 的资源，顺序固定为容器、网络、卷。"""

    now = int(time.time())
    errors: list[str] = []

    for identifier in owned_resource_ids(contract, "container", scope, run_id):
        try:
            name, labels, _ = inspect_labels("container", identifier)
            validate_test_labels(
                contract,
                labels,
                scope=scope,
                now=now,
                expected_run_id=run_id,
            )
            run(["docker", "container", "rm", "-f", identifier])
            print(f"✓ 已清理当前测试容器：{name}")
        except ContractError as error:
            errors.append(str(error))

    for identifier in owned_resource_ids(contract, "network", scope, run_id):
        try:
            name, labels, inspected = inspect_labels("network", identifier)
            validate_test_labels(
                contract,
                labels,
                scope=scope,
                now=now,
                expected_run_id=run_id,
            )
            attached = inspected.get("Containers") or {}
            if attached:
                fail(f"拒绝删除仍有容器附着的网络 {name}")
            run(["docker", "network", "rm", identifier])
            print(f"✓ 已清理当前测试网络：{name}")
        except ContractError as error:
            errors.append(str(error))

    for identifier in owned_resource_ids(contract, "volume", scope, run_id):
        try:
            name, labels, _ = inspect_labels("volume", identifier)
            validate_test_labels(
                contract,
                labels,
                scope=scope,
                now=now,
                expected_run_id=run_id,
            )
            assert_volume_deletable(contract, name, labels)
            run(["docker", "volume", "rm", name])
            print(f"✓ 已清理当前测试卷：{name}")
        except ContractError as error:
            errors.append(str(error))

    if errors:
        fail("当前 run-id 清理失败：" + "；".join(errors))


def cleanup_expired(contract: Contract, scope: str) -> None:
    """只回收已过期且归属完整的资源；任一歧义都会报错并失败关闭。"""

    now = int(time.time())
    errors: list[str] = []

    for identifier in owned_resource_ids(contract, "container", scope):
        try:
            name, labels, _ = inspect_labels("container", identifier)
            _, _, expires_at = validate_test_labels(
                contract,
                labels,
                scope=scope,
                now=now,
            )
            if expires_at > now:
                continue
            run(["docker", "container", "rm", "-f", identifier])
            print(f"✓ 已回收过期测试容器：{name}")
        except ContractError as error:
            errors.append(str(error))

    for identifier in owned_resource_ids(contract, "network", scope):
        try:
            name, labels, inspected = inspect_labels("network", identifier)
            run_id, created_at, expires_at = validate_test_labels(
                contract,
                labels,
                scope=scope,
                now=now,
            )
            if expires_at > now:
                continue
            for container_id in (inspected.get("Containers") or {}):
                _, container_labels, _ = inspect_labels("container", container_id)
                attached_contract = validate_test_labels(
                    contract,
                    container_labels,
                    scope=scope,
                    now=now,
                    expected_run_id=run_id,
                )
                if attached_contract != (run_id, created_at, expires_at):
                    fail(f"网络 {name} 的附着容器时间合同不一致")
                if attached_contract[2] > now:
                    fail(f"网络 {name} 仍附着未过期容器")
            refreshed = docker_json(["network", "inspect", identifier])[0]
            if refreshed.get("Containers"):
                fail(f"网络 {name} 仍有容器附着")
            run(["docker", "network", "rm", identifier])
            print(f"✓ 已回收过期测试网络：{name}")
        except ContractError as error:
            errors.append(str(error))

    for identifier in owned_resource_ids(contract, "volume", scope):
        try:
            name, labels, _ = inspect_labels("volume", identifier)
            _, _, expires_at = validate_test_labels(
                contract,
                labels,
                scope=scope,
                now=now,
            )
            if expires_at > now:
                continue
            assert_volume_deletable(contract, name, labels)
            run(["docker", "volume", "rm", name])
            print(f"✓ 已回收过期测试卷：{name}")
        except ContractError as error:
            errors.append(str(error))

    if errors:
        fail("TTL 回收存在拒绝项：" + "；".join(errors))


def create_network(name: str, labels: dict[str, str]) -> None:
    """创建带完整归属标签的测试网络，不依赖 Compose 项目名前缀。"""

    run(["docker", "network", "create", *label_arguments(labels), name])


def create_volume(name: str, labels: dict[str, str]) -> None:
    """创建唯一临时测试卷；物理名称含随机 run-id，但清理授权只看完整标签。"""

    run(["docker", "volume", "create", *label_arguments(labels), name])


def create_container(
    *,
    name: str,
    image: str,
    network: str,
    volume: str,
    labels: dict[str, str],
    command: str,
    environment: dict[str, str] | None = None,
) -> str:
    """创建持久化测试容器，并把唯一临时卷精确挂载到 /data。"""

    arguments = [
        "docker",
        "container",
        "create",
        "--name",
        name,
        "--network",
        network,
        *label_arguments(labels),
        "--mount",
        f"type=volume,source={volume},target=/data",
    ]
    for key, value in (environment or {}).items():
        arguments.extend(["--env", f"{key}={value}"])
    arguments.extend(["--entrypoint", "/bin/sh", image, "-eu", "-c", command])
    return run(arguments).stdout.strip()


def assert_persistence_mount(container: str, expected_volume: str) -> None:
    """确认专项测试只把预期临时卷挂到 /data，没有额外匿名卷。"""

    mounts = inspect_container(container).get("Mounts") or []
    volume_mounts = [mount for mount in mounts if mount.get("Type") == "volume"]
    if len(volume_mounts) != 1:
        fail(f"持久化测试容器 {container} 必须且只能有 1 个 volume")
    mount = volume_mounts[0]
    if mount.get("Name") != expected_volume or mount.get("Destination") != "/data":
        fail(
            f"持久化测试容器挂载不匹配："
            f"{mount.get('Name')}->{mount.get('Destination')}，期望 {expected_volume}->/data"
        )


def start_attached(container: str) -> str:
    """前台启动容器并返回标准输出；非零退出立即作为合同失败。"""

    return run(["docker", "container", "start", "--attach", container]).stdout


def unique_run_id(prefix: str) -> str:
    """生成只含 Docker 名称安全字符的 run-id，避免并行测试互相覆盖。"""

    return f"{prefix}-{int(time.time())}-{os.getpid()}-{secrets.token_hex(4)}"


def install_signal_handlers() -> dict[int, Any]:
    """安装 INT/TERM 处理器，并返回旧处理器供测试结束后恢复。"""

    previous: dict[int, Any] = {}

    def handle(signum: int, _frame: Any) -> NoReturn:
        raise LifecycleInterrupted(signum)

    for signum in (signal.SIGINT, signal.SIGTERM):
        previous[signum] = signal.getsignal(signum)
        signal.signal(signum, handle)
    return previous


def restore_signal_handlers(previous: dict[int, Any]) -> None:
    """恢复调用前的信号处理器，避免子命令之间相互污染。"""

    for signum, handler in previous.items():
        signal.signal(signum, handler)


def persistence_test(
    contract: Contract,
    *,
    image: str,
    intentional_failure: bool,
    hold_seconds: int,
    ready_file: Path | None,
) -> None:
    """删除第一容器后用同一临时卷重建第二容器，并严格验证随机 sentinel。"""

    cleanup_expired(contract, "persistence-test")
    before = volume_snapshot()
    run_id = unique_run_id("glr-persistence")
    created_at = int(time.time())
    expires_at = created_at + DEFAULT_TTL_SECONDS
    labels = test_labels(
        contract,
        scope="persistence-test",
        run_id=run_id,
        created_at=created_at,
        expires_at=expires_at,
    )
    volume = f"{run_id}-data"
    network = f"{run_id}-network"
    first = f"{run_id}-first"
    second = f"{run_id}-second"
    sentinel = secrets.token_hex(24)
    previous_handlers = install_signal_handlers()
    pending_error: BaseException | None = None

    try:
        create_network(network, labels)
        create_volume(volume, labels)

        first_id = create_container(
            name=first,
            image=image,
            network=network,
            volume=volume,
            labels=labels,
            command='printf "%s" "$SENTINEL" > /data/persistence-sentinel && sync',
            environment={"SENTINEL": sentinel},
        )
        assert_persistence_mount(first_id, volume)
        start_attached(first_id)
        run(["docker", "container", "rm", first_id])

        if ready_file is not None:
            ready_file.write_text(run_id, encoding="utf-8")
        if hold_seconds > 0:
            time.sleep(hold_seconds)
        if intentional_failure:
            fail("按测试要求在第一容器删除后触发故障")

        second_id = create_container(
            name=second,
            image=image,
            network=network,
            volume=volume,
            labels=labels,
            command="cat /data/persistence-sentinel",
        )
        assert_persistence_mount(second_id, volume)
        observed = start_attached(second_id).strip()
        if observed != sentinel:
            fail("第二容器读取的 sentinel 与第一容器写入值不一致")
        run(["docker", "container", "rm", second_id])
        print("✓ 持久化专项证明通过：第一容器已删除，第二容器从同一临时卷读取到 sentinel")
    except BaseException as error:
        pending_error = error
    finally:
        try:
            cleanup_run(contract, "persistence-test", run_id)
            compare_volume_sets(before, volume_snapshot())
        except BaseException as cleanup_error:
            if pending_error is None:
                pending_error = cleanup_error
            else:
                pending_error = ContractError(f"{pending_error}；清理失败：{cleanup_error}")
        restore_signal_handlers(previous_handlers)
        if ready_file is not None:
            ready_file.unlink(missing_ok=True)

    if pending_error is not None:
        raise pending_error


def assert_no_owned_resources(
    contract: Contract,
    scope: str,
    run_id: str | None,
) -> None:
    """最终检查精确标签下没有容器、网络或卷残留。"""

    leftovers: list[str] = []
    for kind in ("container", "network", "volume"):
        for identifier in owned_resource_ids(contract, kind, scope, run_id):
            name, _, _ = inspect_labels(kind, identifier)
            leftovers.append(f"{kind}:{name}")
    if leftovers:
        fail("发现测试资源残留：" + "、".join(leftovers))
    print(f"✓ 标签范围内无残留：scope={scope}, run-id={run_id or '*'}")


def create_abandoned_resources(contract: Contract, image: str) -> NoReturn:
    """创建已过期资源后用 SIGKILL 终止自身，模拟 trap 永远无法执行的场景。"""

    run_id = unique_run_id("glr-sigkill")
    now = int(time.time())
    labels = test_labels(
        contract,
        scope="persistence-test",
        run_id=run_id,
        created_at=now - 2,
        expires_at=now - 1,
    )
    volume = f"{run_id}-data"
    network = f"{run_id}-network"
    container = f"{run_id}-container"
    create_network(network, labels)
    create_volume(volume, labels)
    container_id = create_container(
        name=container,
        image=image,
        network=network,
        volume=volume,
        labels=labels,
        command="sleep 300",
    )
    assert_persistence_mount(container_id, volume)
    run(["docker", "container", "start", container_id])
    print(run_id, flush=True)
    os.kill(os.getpid(), signal.SIGKILL)
    raise AssertionError("SIGKILL 后不可到达")


def sigkill_recovery_test(contract: Contract, manifest: Path, image: str) -> None:
    """启动被 SIGKILL 的子进程，再由下一次 TTL 扫描精确回收其过期资源。"""

    cleanup_expired(contract, "persistence-test")
    before = volume_snapshot()
    completed = subprocess.run(
        [
            sys.executable,
            str(Path(__file__).resolve()),
            "--manifest",
            str(manifest),
            "create-abandoned",
            "--image",
            image,
        ],
        capture_output=True,
        text=True,
    )
    if completed.returncode not in (-signal.SIGKILL, 128 + signal.SIGKILL):
        fail(f"SIGKILL 模拟子进程退出码异常：{completed.returncode}，{completed.stderr.strip()}")
    cleanup_expired(contract, "persistence-test")
    assert_no_owned_resources(contract, "persistence-test", None)
    compare_volume_sets(before, volume_snapshot())
    print("✓ SIGKILL 恢复通过：下一次运行仅回收已过期且归属完整的资源")


def signal_cleanup_test(
    contract: Contract,
    manifest: Path,
    image: str,
    signum: int,
) -> None:
    """向持久化测试发送 INT 或 TERM，并证明 finally 恢复原始卷集合。"""

    cleanup_expired(contract, "persistence-test")
    before = volume_snapshot()
    with tempfile.TemporaryDirectory(prefix="glr-volume-signal-") as directory:
        ready_file = Path(directory) / "ready"
        process = subprocess.Popen(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "--manifest",
                str(manifest),
                "persistence-test",
                "--image",
                image,
                "--hold-seconds",
                "300",
                "--ready-file",
                str(ready_file),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.time() + 60
        while not ready_file.exists() and process.poll() is None and time.time() < deadline:
            time.sleep(0.1)
        if not ready_file.exists():
            stdout, stderr = process.communicate(timeout=10)
            fail(f"信号测试未进入就绪状态：{stdout.strip()} {stderr.strip()}")
        process.send_signal(signum)
        stdout, stderr = process.communicate(timeout=60)
        if process.returncode == 0:
            fail(f"信号 {signum} 未使测试返回非零：{stdout.strip()} {stderr.strip()}")

    assert_no_owned_resources(contract, "persistence-test", None)
    compare_volume_sets(before, volume_snapshot())
    print(f"✓ 信号清理通过：{signal.Signals(signum).name}")


def protected_cleanup_test(contract: Contract) -> None:
    """证明过期测试卷一旦带 protected=true，TTL 清理器会失败且保留该卷。"""

    cleanup_expired(contract, "persistence-test")
    before = volume_snapshot()
    run_id = unique_run_id("glr-protected")
    now = int(time.time())
    labels = test_labels(
        contract,
        scope="persistence-test",
        run_id=run_id,
        created_at=now - 2,
        expires_at=now - 1,
    )
    labels[contract.protected_label] = "true"
    fixture = f"{run_id}-data"
    create_volume(fixture, labels)
    refused = False

    try:
        cleanup_expired(contract, "persistence-test")
    except ContractError as error:
        refused = "protected=true" in str(error)
    if not refused:
        fail("TTL 清理器没有拒绝 protected=true 测试卷")
    if not resource_exists("volume", fixture):
        fail("protected=true 测试卷在拒绝后未保留")

    # 该卷是本函数刚创建的隔离夹具；证明拒绝后，用精确名称显式拆除夹具以恢复全局集合。
    run(["docker", "volume", "rm", fixture])
    compare_volume_sets(before, volume_snapshot())

    fake_labels = test_labels(
        contract,
        scope="persistence-test",
        run_id=run_id,
        created_at=now - 2,
        expires_at=now - 1,
    )
    try:
        assert_volume_deletable(contract, next(iter(contract.approved_names)), fake_labels)
    except ContractError as error:
        if "manifest 运行卷" not in str(error):
            raise
    else:
        fail("精确运行卷名称否决规则未生效")
    print("✓ 保护门禁通过：protected=true 与 manifest 精确名称均失败关闭")


def ttl_safety_test(contract: Contract, image: str) -> None:
    """验证未过期、标签歧义和仍被引用的卷都不会被 TTL 清理器删除。"""

    cleanup_expired(contract, "persistence-test")
    before = volume_snapshot()
    now = int(time.time())

    unexpired_run_id = unique_run_id("glr-unexpired")
    unexpired_labels = test_labels(
        contract,
        scope="persistence-test",
        run_id=unexpired_run_id,
        created_at=now,
        expires_at=now + DEFAULT_TTL_SECONDS,
    )
    unexpired_volume = f"{unexpired_run_id}-data"
    create_volume(unexpired_volume, unexpired_labels)
    cleanup_expired(contract, "persistence-test")
    if not resource_exists("volume", unexpired_volume):
        fail("TTL 清理器错误删除了尚未过期的测试卷")
    cleanup_run(contract, "persistence-test", unexpired_run_id)

    ambiguous_volume = f"{unique_run_id('glr-ambiguous')}-data"
    namespace = contract.test_namespace
    create_volume(
        ambiguous_volume,
        {
            f"{namespace}.project": contract.project,
            f"{namespace}.scope": "persistence-test",
        },
    )
    ambiguous_refused = False
    try:
        cleanup_expired(contract, "persistence-test")
    except ContractError as error:
        ambiguous_refused = "缺少标签" in str(error)
    if not ambiguous_refused or not resource_exists("volume", ambiguous_volume):
        fail("TTL 清理器没有保留并拒绝标签不完整的测试卷")
    run(["docker", "volume", "rm", ambiguous_volume])

    referenced_run_id = unique_run_id("glr-referenced")
    referenced_labels = test_labels(
        contract,
        scope="persistence-test",
        run_id=referenced_run_id,
        created_at=now - 2,
        expires_at=now - 1,
    )
    referenced_volume = f"{referenced_run_id}-data"
    holder = f"{referenced_run_id}-holder"
    create_volume(referenced_volume, referenced_labels)
    run(
        [
            "docker",
            "container",
            "create",
            "--name",
            holder,
            "--network",
            "none",
            "--mount",
            f"type=volume,source={referenced_volume},target=/data",
            "--entrypoint",
            "/bin/sh",
            image,
            "-eu",
            "-c",
            "sleep 300",
        ]
    )
    referenced_refused = False
    try:
        cleanup_expired(contract, "persistence-test")
    except ContractError as error:
        referenced_refused = "仍被容器引用" in str(error)
    if not referenced_refused or not resource_exists("volume", referenced_volume):
        fail("TTL 清理器没有保留并拒绝仍被容器引用的测试卷")
    run(["docker", "container", "rm", "-f", holder])
    run(["docker", "volume", "rm", referenced_volume])

    compare_volume_sets(before, volume_snapshot())
    print("✓ TTL 安全边界通过：未过期、标签歧义、仍被引用的卷均未被自动删除")


def build_parser() -> argparse.ArgumentParser:
    """构建显式子命令，避免脚本根据环境猜测要创建或删除哪些资源。"""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser("provision", help="幂等创建缺失的运行卷")

    guard = subparsers.add_parser("guard", help="守卫任意命令前后的 Docker volume 全局集合")
    guard.add_argument("guarded_command", nargs=argparse.REMAINDER)

    audit = subparsers.add_parser("audit-compose", help="审计运行与普通测试 Compose 合同")
    audit.add_argument("--runtime-compose", type=Path, default=PROJECT_ROOT / "docker-compose.yml")
    audit.add_argument("--test-compose", type=Path, default=PROJECT_ROOT / "docker-compose.test.yml")

    mounts = subparsers.add_parser("assert-no-volume-mounts", help="检查普通测试容器真实挂载")
    mounts.add_argument("--container", required=True)
    mounts.add_argument("--require-tmpfs", action="append", default=[])

    cleanup = subparsers.add_parser("cleanup-expired", help="精确回收已过期测试资源")
    cleanup.add_argument("--scope", choices=("ordinary-test", "persistence-test"), required=True)

    no_owned = subparsers.add_parser("assert-no-owned-resources", help="检查标签范围内没有资源残留")
    no_owned.add_argument("--scope", choices=("ordinary-test", "persistence-test"), required=True)
    no_owned.add_argument("--run-id")

    persistence = subparsers.add_parser("persistence-test", help="执行临时卷跨容器重建持久化测试")
    persistence.add_argument("--image", default=DEFAULT_IMAGE)
    persistence.add_argument("--intentional-failure", action="store_true")
    persistence.add_argument("--hold-seconds", type=int, default=0)
    persistence.add_argument("--ready-file", type=Path)

    abandoned = subparsers.add_parser("create-abandoned", help=argparse.SUPPRESS)
    abandoned.add_argument("--image", default=DEFAULT_IMAGE)

    sigkill = subparsers.add_parser("sigkill-recovery-test", help="验证 SIGKILL 后的 TTL 恢复")
    sigkill.add_argument("--image", default=DEFAULT_IMAGE)

    signal_test = subparsers.add_parser("signal-cleanup-test", help="验证 INT/TERM 清理")
    signal_test.add_argument("--image", default=DEFAULT_IMAGE)
    signal_test.add_argument("--signal", choices=("INT", "TERM"), required=True)

    subparsers.add_parser("protected-cleanup-test", help="验证运行卷双重保护门禁")
    ttl_safety = subparsers.add_parser("ttl-safety-test", help="验证 TTL 拒绝边界")
    ttl_safety.add_argument("--image", default=DEFAULT_IMAGE)
    return parser


def main() -> int:
    """分派子命令，并把合同失败转换为稳定非零退出码。"""

    try:
        parser = build_parser()
        args = parser.parse_args()
        manifest = args.manifest.resolve()
        contract = load_contract(manifest)
        if args.command == "provision":
            provision(contract)
        elif args.command == "guard":
            return guard_command(args.guarded_command)
        elif args.command == "audit-compose":
            audit_compose(
                contract,
                args.runtime_compose.resolve(),
                args.test_compose.resolve(),
            )
        elif args.command == "assert-no-volume-mounts":
            assert_no_volume_mounts(args.container, tuple(args.require_tmpfs))
        elif args.command == "cleanup-expired":
            cleanup_expired(contract, args.scope)
        elif args.command == "assert-no-owned-resources":
            assert_no_owned_resources(contract, args.scope, args.run_id)
        elif args.command == "persistence-test":
            if args.hold_seconds < 0:
                fail("--hold-seconds 不能为负数")
            persistence_test(
                contract,
                image=args.image,
                intentional_failure=args.intentional_failure,
                hold_seconds=args.hold_seconds,
                ready_file=args.ready_file,
            )
        elif args.command == "create-abandoned":
            create_abandoned_resources(contract, args.image)
        elif args.command == "sigkill-recovery-test":
            sigkill_recovery_test(contract, manifest, args.image)
        elif args.command == "signal-cleanup-test":
            signum = signal.SIGINT if args.signal == "INT" else signal.SIGTERM
            signal_cleanup_test(contract, manifest, args.image, signum)
        elif args.command == "protected-cleanup-test":
            protected_cleanup_test(contract)
        elif args.command == "ttl-safety-test":
            ttl_safety_test(contract, args.image)
        else:
            parser.error(f"未知子命令：{args.command}")
    except LifecycleInterrupted as error:
        print(f"⚠ {error}，当前测试资源已进入清理流程。", file=sys.stderr)
        return 128 + error.signum
    except ContractError as error:
        print(f"✗ Docker 卷合同失败：{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
