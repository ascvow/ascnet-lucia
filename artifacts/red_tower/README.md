# 红塔制作记录

## 输入与边界
参考为用户提供的设定图 reference.png；项目中未发现独立原始场景图。底部环境小图与正面设定仅用于视觉参考，不能视为严格工程投影。高度 100 单位是工作尺度假设，不代表真实米数。正面 -Y，竖直 +Z。

## 阶段 A 分析
实体：收分的连续纵脊、横向开放片层、断梁、底部斜向扶壁、顶部短冠部。
可能脱落：局部断梁附近的孤立暗色碎片，集中布置于三个破损带且可关闭。
视觉效果：贯穿天空的横向亮线、光晕、云旋涡和像素亮斑不挤出为几何。
分区：0–20 为破损扶壁和宽下段，20–65 为收分主段，65–94 为纤细上段，94–102 为不对称顶部。
已知：细长剪影、红色纵向槽、密集但缺损的横层。补全：六脊连接、背面护甲和开放层架的内部支承。
几何负责剪影、切口和厚度；材质区分钢、断面、建筑残块与能量；合成仅提供可关闭的轻微辉光。

## 环境
Blender /Applications/Blender.app/Contents/MacOS/Blender，5.2.1 LTS。
检测设备：Apple M4 Pro CPU、Apple M4 Pro Metal GPU（16 核）。
受限启动曾在初始化崩溃；正常本机权限启动成功。
本任务使用实际 Blender 渲染和图片查看工具进行验收，不采用二维生成图替代资产。

## 复现
以下为阶段调用形式；最终空场景复现命令见下一节：
```sh
/Applications/Blender.app/Contents/MacOS/Blender --background --factory-startup --python artifacts/red_tower/build.py -- A
/Applications/Blender.app/Contents/MacOS/Blender --background --factory-startup --python artifacts/red_tower/build.py -- B
```
parameters.json 集中控制高度分区、片层数量、破损带、种子、发光、分辨率和采样。
脚本仅删除并重建 RT_ 前缀对象，不清空用户场景。请先为已有工程另存副本。
独立集合：骨架、片层、底座、能量、碎片、环境、摄影。
## 最终复现顺序
先执行 build.py 的 B 阶段，再执行 refine.py，最后执行 verify.py。使用空启动场景，避免 Blender 默认立方体混入底座：
```sh
/Applications/Blender.app/Contents/MacOS/Blender -b --factory-startup --python-expr "import bpy; bpy.ops.wm.read_factory_settings(use_empty=True)" --python-exit-code 1 --python artifacts/red_tower/build.py -- B
/Applications/Blender.app/Contents/MacOS/Blender -b --factory-startup --python-exit-code 1 --python artifacts/red_tower/refine.py
/Applications/Blender.app/Contents/MacOS/Blender -b --factory-startup --python-exit-code 1 --python artifacts/red_tower/verify.py
```
refine.py 始终以 stage_B.blend 为输入，重复执行不会叠加材质节点或倒角。
build.py 管理 RT_ 前缀对象和检查场景；重复生成保持固定种子，不复制多套塔。
parameters.json 的 profile 为高度与半径控制点，其他构件的制作参数集中于脚本对应分区。当前尺度固定为约 100 单位；不要仅改 profile 的高度就假定其他附件会同比缩放。
骨架、横层等基础盒构件共享网格；独立破损片层保留网格编辑能力。能量夹具与梁端存在有意的装配搭接。

## 视觉迭代证据
1. A1_front、A1_side、A1_hero：中上段间距过大、底部扶壁过整齐、侧面外伸消失。原因是几何而非相机。将灰模层数 57 增至 110，底座展开 10–15 改为 9–13，扶壁边线加入阶梯断裂，并增加 Y 方向断梁。A2 的层密度和侧面剪影改善，透视出现上段宽扇叶。
2. A2_hero 上半部：外伸片层成周期性宽扇叶。缩小 40 单位以上的外伸系数，并将 60 以上的额外外伸乘 0.32；补充少量长短壳体肋板。A3_hero 保持相机、照明和灰材质，上段宽扇叶减轻。仍可见较规则主格架，进入中尺度结构细化。
3. v1_middle、v1_base：片层边缘完整、发光窗口形成同宽梯子，底部是规则方砖。添加片层外缘缺口与高度偏移、不同宽度能量窗、48 块有阶梯断口的纵向装甲，底部石块改为不等顶点破碎体。v2_middle 的破损带与装甲清晰，v2_base 不再是一圈完整砖块；局部碎石悬空是新问题。
4. v2_base 前景：碎石落点未随旋转更新。按世界空间最低顶点落地；对碎石三角化，重算法线，按构件尺度加倒角。base_final 中前景接触改善。残存片层结构仍偏规整，没有把这一问题归为照明。
5. v2_middle 与 middle_final：保持灯光、曝光、相机；钢与断面使用低幅度色差、粗糙度和浅凹凸。最终大面仍能读出，没有以强噪声掩盖结构。塔顶高亮限定于小型核心，塔身横层仍可辨。
6. 最终重开 red_tower.blend 后生成 check_front、check_side、check_back、check_perspective、check_emission。前四张关闭环境、漂浮碎片、材质发光与合成，使用灰材质。实际查看了这些图片。首次重开发现未被活动窗口引用的检查场景未保存，补充持久标志后重开确认三个场景均存在。

相机从 A1 至最终保持固定；A1–A3 的正交图有地面边界，最终 check 系列关闭环境以排除该干扰。没有将不同相机图片用于证明几何比例改善。

## 交付清单与编辑
- red_tower.blend：默认 RT_展示；另有 RT_结构检查、RT_发光无辉光，可从场景菜单切换。
- red_tower_2048.png：1474 × 2048，实际 Cycles 渲染；只有轻微合成辉光，没有二维补绘。
- check_front.png、check_side.png、check_back.png：同一正交尺度 112，737 × 1024，无环境和特效。
- check_perspective.png：同一参考匹配相机的灰模图。
- check_emission.png：有发光、无辉光。
- base_final.png、middle_final.png、top_final.png：1024 方形关键近景，无辉光。
- stage_A_v1/v2/v3.blend、stage_B_v1/v2.blend、stage_C.blend：阶段工程。
- A1/A2/A3 图片以及 v1/v2 图片：原始阶段对照，未后期涂改。
- validation.json：最终工程重开后的技术检查数据。

材质发光由 RT_红色能量和 RT_塔顶能量的 Emission Strength 控制；辉光由 RT_合成节点组 Glare 的 Strength 控制；曝光由场景 Color Management 控制。环境仅有地面，没有雾、云层、景深或运动模糊。检查场景共享主体对象，修改主体会同步反映于各场景。

## 验证与实际预算
Blender 5.2.1 LTS，Cycles，Metal GPU，最终 48 采样上限、自适应阈值 0.025、降噪开启；重开正交检查 32 采样。首次 Metal 初始化较慢，后续 1024 检查约每张 1.4–2.9 秒。未机械提高采样数。
最终检查：1502 个网格对象、952 个独立网格、9607 个基础面；面数仅为编辑成本信息，不作为质量标准。Mesh.validate 临时副本未报告损坏，未发现零面积面、非有限顶点、缺失外部贴图；倒角参数检查通过。已检查实际近景，未发现明显闪烁或法线反转；未进行工业级全对象碰撞求解。
降噪后的细长断梁仍可见，1024 全塔图无法用于微小夹具验收，应看局部图或工程。

## 未解决差距
该版本是实际可编辑、可复现的三维资产，但不能声称已达到参考图的高还原度成品标准。主要差距是整体仍偏规整层架，参考中复杂的非对称结构堆叠、巨大废墟基部和塔顶冠部尚未充分还原。顶部焦点较克制，未重建云层旋涡与镜头眩光。背面与内部为一致性补全，缺少独立原始场景图，不能视为已确认设计。
保留当前可用版本和阶段证据，不以更多随机碎片或更强红光掩盖这些差距。

