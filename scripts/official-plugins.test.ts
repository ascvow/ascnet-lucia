import { afterEach, expect, test } from 'bun:test'
import { copyFile, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { loadOfficialPluginCatalog } from './official-plugins'

const fixtures: string[] = []
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((path) => rm(path, { recursive: true, force: true })))
})

/** 复制真实安装脚本到隔离仓库；所有写入都在临时目录，不接触用户 Lucia Home。 */
async function fixture(retired?: string[]) {
  const root = await mkdtemp(join(tmpdir(), 'lucia-plugin-distribution-'))
  fixtures.push(root)
  await mkdir(join(root, 'scripts'))
  await mkdir(join(root, 'registry'))
  for (const name of ['official-plugins.ts', 'install-official-plugins.ts']) {
    await copyFile(join(import.meta.dir, name), join(root, 'scripts', name))
  }
  const catalog = {
    schema_version: 1,
    publisher: 'test',
    release: { owner: 'test', repository: 'test', tag: 'test' },
    ...(retired === undefined ? {} : { retired }),
    plugins: [{ id: 'plan', crate: 'plan-plugin', directory: 'examples/plan', files: ['plugin.toml', 'plan.wasm'] }],
  }
  await writeFile(join(root, 'registry', 'official-plugins.json'), JSON.stringify(catalog))
  await mkdir(join(root, 'examples', 'plan'), { recursive: true })
  await writeFile(join(root, 'examples', 'plan', 'plugin.toml'), '[plugin]\nid="plan"\n')
  await writeFile(join(root, 'examples', 'plan', 'plan.wasm'), 'new-wasm')
  return { root, home: join(root, 'home') }
}

/** 真实执行安装入口并收集退出码；HOME 始终为隔离测试目录。 */
async function install(root: string, home: string) {
  const child = Bun.spawn([process.execPath, 'run', 'scripts/install-official-plugins.ts'], {
    cwd: root, env: { ...process.env, LUCIA_HOME: home }, stdout: 'pipe', stderr: 'pipe',
  })
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited, new Response(child.stdout).text(), new Response(child.stderr).text(),
  ])
  return { exitCode, stdout, stderr }
}

test('默认构建、安装、打包共享清单且不再包含权限插件', async () => {
  const catalog = await loadOfficialPluginCatalog()
  expect(catalog.plugins.map((plugin) => plugin.id)).toEqual(['mcp', 'teammate', 'plan', 'memory', 'knowledge', 'eden'])
  expect(catalog.retired).toEqual(['permission', 'sandbox'])
  const { scripts } = await Bun.file(join(import.meta.dir, '..', 'package.json')).json()
  expect(scripts['build:plugin:all']).toBe('bun run build:plugin:official && bun run build:plugin:examples')
  expect(scripts['test:plugin:official']).not.toContain('permission')
  expect(scripts['build:plugin:examples']).not.toContain('permission')
  expect(scripts['build:plugin:permission']).toContain('permission-plugin')
  for (const script of ['build-official-plugins.ts', 'install-official-plugins.ts', 'package-official-plugins.ts']) {
    expect(await Bun.file(join(import.meta.dir, script)).text()).toContain('loadOfficialPluginCatalog')
  }
})

test('旧清单缺少 retired 时仍能安装且不动未管理目录', async () => {
  const { root, home } = await fixture()
  await mkdir(join(home, 'official-plugins', 'custom'), { recursive: true })
  await writeFile(join(home, 'official-plugins', 'custom', 'config.toml'), 'user-config')
  expect((await install(root, home)).exitCode).toBe(0)
  expect(await readFile(join(home, 'official-plugins', 'plan', 'plan.wasm'), 'utf8')).toBe('new-wasm')
  expect(await readFile(join(home, 'official-plugins', 'custom', 'config.toml'), 'utf8')).toBe('user-config')
})

test('退役 bundle 移出扫描目录并完整保留，重复安装不覆盖备份', async () => {
  const { root, home } = await fixture(['permission', 'sandbox'])
  for (const id of ['permission', 'sandbox', 'plan']) {
    await mkdir(join(home, 'official-plugins', id), { recursive: true })
    await writeFile(join(home, 'official-plugins', id, 'config.toml'), `${id}-settings`)
  }
  await mkdir(join(home, 'plugins', 'permission'), { recursive: true })
  await writeFile(join(home, 'plugins', 'permission', 'config.toml'), 'manual-install')
  await writeFile(join(home, 'config.json'), '{"do_not_change":true}')
  const first = await install(root, home)
  expect(first.exitCode).toBe(0)
  expect(first.stdout).toContain('已停用退役官方插件：permission')
  expect(await readdir(join(home, 'official-plugins'))).toEqual(['plan'])
  const backups = (await readdir(join(home, 'disabled-official-plugins'))).sort()
  expect(backups.length).toBe(2)
  for (const id of ['permission', 'sandbox']) {
    const backup = backups.find((name) => name.startsWith(`${id}-`))!
    expect(await readFile(join(home, 'disabled-official-plugins', backup, id, 'config.toml'), 'utf8')).toBe(`${id}-settings`)
  }
  expect((await install(root, home)).exitCode).toBe(0)
  expect((await readdir(join(home, 'disabled-official-plugins'))).sort()).toEqual(backups)
  expect(await readFile(join(home, 'official-plugins', 'plan', 'config.toml'), 'utf8')).toBe('plan-settings')
  expect(await readFile(join(home, 'plugins', 'permission', 'config.toml'), 'utf8')).toBe('manual-install')
  expect(await readFile(join(home, 'config.json'), 'utf8')).toBe('{"do_not_change":true}')
})

test('拒绝越界、重复和仍在分发的退役 ID，不执行迁移', async () => {
  for (const retired of [['../outside'], ['permission', 'permission'], ['plan']]) {
    const { root, home } = await fixture(retired)
    const result = await install(root, home)
    expect(result.exitCode).not.toBe(0)
    expect(result.stderr).toContain('退役关系无效')
    expect(await Bun.file(join(home, 'official-plugins', 'plan', 'plan.wasm')).exists()).toBe(false)
  }
})
