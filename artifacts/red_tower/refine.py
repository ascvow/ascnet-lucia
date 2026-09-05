"""从已保存 B 工程继续精修；不重建主体，保留独立阶段文件。"""
import bpy, bmesh, math, json, random
from pathlib import Path
from mathutils import Vector
ROOT=Path(__file__).resolve().parent
P=json.loads((ROOT/'parameters.json').read_text())
bpy.ops.wm.open_mainfile(filepath=str(ROOT/'stage_B.blend'))
SC=bpy.context.scene
if 'RT_检查灰' not in bpy.data.materials:
    gray=bpy.data.materials.new('RT_检查灰');gray.use_nodes=True;gray.use_fake_user=True
    gray.node_tree.nodes.get('Principled BSDF').inputs['Base Color'].default_value=(.36,.36,.36,1)
# 修正所有本任务网格法线；碎石明确三角化，消除非共面四边形的隐式折线。
seen=set()
for o in bpy.data.objects:
    if not o.name.startswith('RT_') or o.type!='MESH' or o.data.name in seen:continue
    seen.add(o.data.name)
    bm=bmesh.new();bm.from_mesh(o.data)
    if '基底断块' in o.name:bmesh.ops.triangulate(bm,faces=list(bm.faces))
    bmesh.ops.recalc_face_normals(bm,faces=list(bm.faces))
    bm.to_mesh(o.data);bm.free()
# 尺度相关倒角：网格空间与物体缩放分开，避免细薄片层被圆化。
for o in bpy.data.objects:
    if o.type!='MESH' or not o.name.startswith('RT_'):continue
    if o.users_collection[0].name not in ['RT_骨架','RT_底座','RT_片层']:continue
    if any(m.type=='BEVEL' for m in o.modifiers):continue
    b=o.modifiers.new('尺度匹配边缘倒角','BEVEL');b.segments=2;b.limit_method='ANGLE'
    b.width=.012 if '片层' in o.name else (.025 if o.data.users>1 else .03)
# 碎石底面按世界包围顶点落地；避免前景明显悬空。
for o in bpy.data.objects:
    if not o.name.startswith('RT_基底断块'):continue
    bpy.context.view_layer.update()
    low=min((o.matrix_world@v.co).z for v in o.data.vertices)
    o.location.z-=low+.085
bpy.ops.wm.save_as_mainfile(filepath=str(ROOT/'stage_B_v2.blend'))
# 材质仅使用低幅度、多尺度变化；用几何法线区分积尘顶面与暗色侧面。
for name in ['氧化钢','边缘钢','断裂截面','残存混凝土']:
    m=bpy.data.materials['RT_'+name];nt=m.node_tree;b=nt.nodes.get('Principled BSDF')
    coord=nt.nodes.new('ShaderNodeTexCoord');noise=nt.nodes.new('ShaderNodeTexNoise')
    noise.inputs['Scale'].default_value=5 if name=='残存混凝土' else 2.8
    noise.inputs['Detail'].default_value=2.4
    nt.links.new(coord.outputs['Object'],noise.inputs['Vector'])
    ramp=nt.nodes.new('ShaderNodeValToRGB')
    if name=='氧化钢': colors=[(.035,.031,.029,1),(.10,.075,.056,1)]
    elif name=='边缘钢': colors=[(.075,.073,.07,1),(.17,.155,.133,1)]
    elif name=='断裂截面': colors=[(.075,.042,.025,1),(.23,.14,.08,1)]
    else:colors=[(.12,.117,.11,1),(.24,.22,.185,1)]
    ramp.color_ramp.elements[0].position=.2;ramp.color_ramp.elements[0].color=colors[0]
    ramp.color_ramp.elements[1].position=.8;ramp.color_ramp.elements[1].color=colors[1]
    nt.links.new(noise.outputs['Fac'],ramp.inputs[0]);nt.links.new(ramp.outputs['Color'],b.inputs['Base Color'])
    rough=nt.nodes.new('ShaderNodeMapRange');rough.inputs['To Min'].default_value=.52;rough.inputs['To Max'].default_value=.8
    nt.links.new(noise.outputs['Fac'],rough.inputs[0]);nt.links.new(rough.outputs[0],b.inputs['Roughness'])
    bump=nt.nodes.new('ShaderNodeBump');bump.inputs['Strength'].default_value=.12;bump.inputs['Distance'].default_value=.022
    nt.links.new(noise.outputs['Fac'],bump.inputs['Height']);nt.links.new(bump.outputs[0],b.inputs['Normal'])
bpy.ops.wm.save_as_mainfile(filepath=str(ROOT/'stage_C.blend'))
# 保存可直接切换的检查场景；共享几何，各场景仅覆盖检查状态。
SC.name='RT_展示'
SC.camera=bpy.data.objects['RT_参考透视']
SC.render.resolution_x=1474;SC.render.resolution_y=2048
SC.render.image_settings.file_format='PNG'
SC.cycles.samples=48;SC.cycles.adaptive_threshold=.025
SC.render.filepath=str(ROOT/'red_tower_2048.png')
SC.view_settings.exposure=0
SC['发光控制']='RT_红色能量和 RT_塔顶能量材质的 Emission Strength'
SC['辉光控制']='RT_合成节点组的 Glare Strength，默认 0.14'
SC['说明']='灰模和无辉光检查场景共享本体几何；环境集合可独立关闭'
for name in ['RT_结构检查','RT_发光无辉光']:
    if name in bpy.data.scenes:bpy.data.scenes.remove(bpy.data.scenes[name])
    cp=SC.copy();cp.name=name;cp.use_fake_user=True
    cp.compositing_node_group=None
    cp.camera=bpy.data.objects['RT_正面']
    cp.render.resolution_x=737;cp.render.resolution_y=1024
    if name=='RT_结构检查':
        cp.view_layers[0].material_override=bpy.data.materials['RT_检查灰']
        for lc in cp.view_layers[0].layer_collection.children:
            if lc.name in ['RT_环境','RT_碎片']:lc.exclude=True
# 工程默认是无额外雾的完整展示，所有外部数据均无贴图依赖。
bpy.context.window.scene=SC
bpy.ops.wm.save_as_mainfile(filepath=str(ROOT/'red_tower.blend'))
for view in ['中段近景','塔底近景','塔顶近景']:
    SC.camera=bpy.data.objects['RT_'+view];SC.compositing_node_group=None
    SC.render.resolution_x=1024;SC.render.resolution_y=1024
    SC.render.filepath=str(ROOT/({'中段近景':'middle_final.png','塔底近景':'base_final.png','塔顶近景':'top_final.png'}[view]))
    bpy.ops.render.render(write_still=True)
SC.camera=bpy.data.objects['RT_参考透视'];SC.compositing_node_group=bpy.data.node_groups['RT_合成']
SC.render.resolution_x=1474;SC.render.resolution_y=2048
SC.render.filepath=str(ROOT/'red_tower_2048.png')
bpy.ops.wm.save_as_mainfile(filepath=str(ROOT/'red_tower.blend'))
bpy.ops.render.render(write_still=True)
