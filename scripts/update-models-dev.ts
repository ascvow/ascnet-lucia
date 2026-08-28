import { mkdir } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'

const sourceUrl = 'https://models.dev/api.json'
const outputPath = resolve(import.meta.dir, '../crates/agent-tui/assets/models-dev.json')

type ModelRecord = {
  id?: unknown
  reasoning?: unknown
  reasoning_options?: unknown
  limit?: { context?: unknown; output?: unknown }
  modalities?: { input?: unknown; output?: unknown }
}

/** 校验打包依赖的 models.dev 字段，拒绝把错误页或不兼容 schema 嵌入二进制。 */
function validateSnapshot(value: unknown): asserts value is Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('models.dev 快照根节点必须是服务商对象')
  }
  let modelCount = 0
  for (const [providerId, provider] of Object.entries(value)) {
    if (!provider || typeof provider !== 'object' || Array.isArray(provider)) {
      throw new Error(`models.dev 服务商 ${providerId} 不是对象`)
    }
    const models = (provider as { models?: unknown }).models
    if (!models || typeof models !== 'object' || Array.isArray(models)) {
      throw new Error(`models.dev 服务商 ${providerId} 缺少 models 对象`)
    }
    for (const [modelId, rawModel] of Object.entries(models)) {
      const model = rawModel as ModelRecord
      if (!model || typeof model !== 'object' || model.id !== modelId) {
        throw new Error(`models.dev 模型键与 id 不一致：${providerId}/${modelId}`)
      }
      if (typeof model.reasoning !== 'boolean' || !model.limit || typeof model.limit !== 'object') {
        throw new Error(`models.dev 模型缺少能力字段：${providerId}/${modelId}`)
      }
      for (const [name, limit] of Object.entries(model.limit)) {
        if (
          ['context', 'output'].includes(name) &&
          limit !== undefined &&
          typeof limit !== 'number'
        ) {
          throw new Error(`models.dev 模型 ${providerId}/${modelId} 的 ${name} 上限无效`)
        }
      }
      if (!model.modalities || typeof model.modalities !== 'object') {
        throw new Error(`models.dev 模型缺少输入输出模态：${providerId}/${modelId}`)
      }
      for (const direction of ['input', 'output'] as const) {
        const modalities = model.modalities[direction]
        if (!Array.isArray(modalities) || modalities.some((item) => typeof item !== 'string')) {
          throw new Error(`models.dev 模型 ${providerId}/${modelId} 的 ${direction} 模态无效`)
        }
      }
      modelCount += 1
    }
  }
  if (modelCount === 0) throw new Error('models.dev 快照不包含模型')
}

/** 下载、校验并以稳定 JSON 格式写入 TUI 编译期快照。 */
async function main(): Promise<void> {
  const response = await fetch(sourceUrl)
  if (!response.ok) throw new Error(`下载 models.dev 快照失败：HTTP ${response.status}`)
  const snapshot: unknown = await response.json()
  validateSnapshot(snapshot)
  await mkdir(dirname(outputPath), { recursive: true })
  await Bun.write(outputPath, `${JSON.stringify(snapshot)}\n`)
  const bytes = new Uint8Array(await Bun.file(outputPath).arrayBuffer())
  const sha256 = new Bun.CryptoHasher('sha256').update(bytes).digest('hex')
  console.log(`已更新 models.dev 快照：${outputPath}（SHA-256 ${sha256}）`)
}

await main()
