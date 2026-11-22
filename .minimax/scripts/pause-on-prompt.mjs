#!/usr/bin/env node
// Project Hook: UserPromptSubmit —— 用户提交新消息时**立即暂停《缺氧》游戏**。
//
// 为什么需要：主 agent 的思考要几分钟，游戏不会等你。写操作/规划期间让世界
// 继续跑，等于让复制人在你思考时窒息或饿死。暂停把世界冻结成一张可静态分析
// 的快照，等规划好再统一恢复。
//
// 约束：
// - 项目级 Hook（<project>/.minimax/hooks.json）→ 只在本仓库的会话里生效。
// - 失败必须静默：Hook 不能阻塞用户对话（fail-open）。
// - 已暂停时是幂等的，不报错。

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// 按**脚本自身位置**定位仓库（cwd 无关）。
//   旧逻辑只认 cwd / $HOME/dev/oni-agent → hook 运行目录一变就找不到二进制，
//   而失败是 fail-open 的 → 用户新消息**没有冻结世界**。
const HERE = dirname(fileURLToPath(import.meta.url));
const SELF_REPO = join(HERE, '..', '..');   // <repo>/.minimax/scripts → <repo>

const CANDIDATE_REPOS = [
  process.env.ONI_AGENT_REPO,
  SELF_REPO,
  process.cwd(),
  join(process.env.HOME || '', 'dev', 'oni-agent'),
].filter(Boolean);

function findOniBinary() {
  for (const repo of CANDIDATE_REPOS) {
    const bin = join(repo, 'target', 'release', 'oni');
    if (existsSync(bin)) return { bin, repo };
  }
  return null;
}

function run() {
  const found = findOniBinary();
  if (!found) process.exit(0);
  const child = spawn(
    found.bin,
    ['pause', '用户新消息：冻结世界以便观察局势并重新规划'],
    { cwd: found.repo, stdio: 'ignore' }
  );
  // 兜底：最多等 8s，超时就杀掉，绝不拖住用户
  const kill = setTimeout(() => {
    try { child.kill('SIGKILL'); } catch {}
    process.exit(0);
  }, 8000);
  const done = () => { clearTimeout(kill); process.exit(0); };
  child.on('error', done);
  child.on('close', done);
}

let fired = false;
function once() {
  if (fired) return;
  fired = true;
  run();
}

try {
  const chunks = [];
  process.stdin.on('data', (c) => chunks.push(c));
  process.stdin.on('end', once);
  process.stdin.on('error', once);
  // stdin 永不结束时也要退出
  setTimeout(once, 3000).unref();
} catch {
  once();
}