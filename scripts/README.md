# GetLatestRepo 脚本

---

## 脚本列表

| 脚本 | 功能 | 用法 |
|------|------|------|
| `install.sh` | 安装 getlatestrepo 到系统 | `./install.sh` |
| `release.sh` | 构建并验证 release 产物 | `./release.sh` |
| `clean-reports.sh` | 清理过期报告 | `./clean-reports.sh 30` |
| `docker-volume-provision.sh` | 幂等创建 manifest 中缺失的 external 运行卷 | `./docker-volume-provision.sh` |
| `docker-test-ordinary.sh` | 执行 tmpfs 普通零卷测试 | `./docker-test-ordinary.sh` |
| `verify-docker-volumes-ordinary.sh` | 验证普通测试零卷 lane | `./verify-docker-volumes-ordinary.sh` |
| `verify-docker-volumes-persistence.sh` | 验证持久化专项 lane | `./verify-docker-volumes-persistence.sh` |
| `verify-docker-volumes.sh` | 验证完整 Docker 卷生命周期合同 | `./verify-docker-volumes.sh` |

---

## Docker 卷安全边界

- 运行卷只认 `docker/volumes.manifest.json` 中的精确名称。
- 项目自动化禁止执行全局 `docker volume prune`。
- 测试清理只认完整 project、scope、run-id 与时间标签。
- 普通测试禁止 named/anonymous volume；持久化测试必须使用独立命令。
- 任意测试结束后，Docker volume 全局集合必须与测试前完全一致。

---
