# 红塔第二版：细节、灯光与后期

本轮在第一版实体上继续细化，原版完整归档为 source.blend。当前可直接打开的工程是上级目录 red_tower.blend；本目录的 beauty_2560.png 为最新展示图，beauty_raw_2560.png 为同场景、同曝光、同尺寸且无合成后期的原始渲染。

## 细节提升
- 新增沿高度收分的核心导管束及固定压板，集中高度区域的连接法兰、铆钉和立面连接肋。
- 新增有实际厚度、根部斜撑和连接块的断口托板，未改变原有主轴、整体比例与参考匹配相机。
- 塔顶新增不对称冠爪、开放八角框和引流管；底部增加锚固底板、螺帽和局部裸露钢筋。
- 新增盒状构件共享网格，曲线导管保留可编辑半径；细节分为四个 RT2_ 集合。没有为了面数将曲线全部实体化。

## 灯光与效果
独立 RT2_展示灯光集合包含冷色主光和轮廓光、较暖填充光、七个核心局部红色反照灯与塔顶焦点灯。
RT2_大气效果为低密度体积大气，RT2_展示弧形背景用连续几何连接地面，去除原预览的硬横向分界。
RT2_展示后期为 Blender 5.2 合成节点组：柔和辉光与塔顶星芒分别控制。曝光保持 0；没有后期补绘，没有把光线直接挤出为实体。
RT_结构检查场景关闭展示灯光、大气和背景，保留原中性灯光；新增细节实体仍可见。RT_发光无辉光保留原中性灯光及真实材质。

## 实际视觉迭代
1. before_middle 与 detail_middle：固定原相机、原中性灯光和灰材质。旧版核心两侧为空的直壁；新版增加连续三联导管、压板和局部连接构件，中距离能够读出连续路径。detail_top 增加冠框层次，detail_base 的锚座和钢筋与现有扶壁聚集。原有整体收分不变。
2. v1_style_preview 与 v1_style_raw：第一轮冷暖灯光、红色核心反照与低密度大气。实际查看后发现塔身暗部偏重，背景中下部有明显水平分界。辉光仅集中在塔顶，没有大范围吞没片层。
3. style_preview 与 style_raw：主体和相机保持不变；主光 48000→78000、填充光 24000→42000、核心反照 130→240；增加连续弧形背景。预览中横向分界消失，中下部支架更容易辨认。红色发光强度和曝光未随这一轮灯光调整改变。
4. 最终展示与无后期原始图为同一工程重开后的实际渲染。近景关闭辉光，检查实体连接和边缘。技术报告位于 validation.json。实际查看最终展示图、三个近景与三视图；新增管线、冠框和锚座仍能辨认。塔顶近景背景有少量体积噪声，未用强降噪继续抹平。

## 复现
已安装 Blender 5.2.1 LTS，使用 Apple M4 Pro Metal GPU。
从项目根目录依次运行：
```sh
/Applications/Blender.app/Contents/MacOS/Blender -b --factory-startup --python-exit-code 1 --python artifacts/red_tower/upgrade.py -- detail
/Applications/Blender.app/Contents/MacOS/Blender -b --factory-startup --python-exit-code 1 --python artifacts/red_tower/upgrade.py -- style
/Applications/Blender.app/Contents/MacOS/Blender -b --factory-startup --python-exit-code 1 --python artifacts/red_tower/deliver_upgrade.py
```
detail 固定从 source.blend 读取，style 固定从 detail.blend 读取；因此重复运行不会累积对象或灯光。deliver_upgrade.py 从 candidate.blend 读取，保存上级 red_tower.blend 后重新打开并渲染。
settings.json 集中管理种子、灯光功率、核心发光、辉光、星芒、大气密度、采样和输出尺寸。中尺度几何参数保留在 upgrade.py 对应分区，便于直接调整锚点。
最终从保存工程重开验证通过：无损坏网格、零面积面、非有限顶点或缺失图片；完整展示实际耗时 154.25 秒。
最终 Cycles 上限 64 采样、自适应阈值 0.018、降噪开启；展示 1843 × 2560，近景 1440 × 1440，三视图 864 × 1200。

## 输出与边界
- beauty_2560.png、beauty_raw_2560.png：最终有无后期对照。
- middle_lit.png、top_lit.png、base_lit.png：升级后的照明近景，无合成后期。
- check_front.png、check_side.png、check_back.png、check_perspective.png：原中性灯光、无环境与发光的结构检查。
- source.blend、detail.blend、v1_candidate.blend、candidate.blend：输入及阶段版本。
这是对现有资产的细节和展示升级，不代表已完成原参考中复杂废墟、旋涡云层和全部非对称破损的还原。当前背景为资产展示布景，不宣称重建了原始场景。
