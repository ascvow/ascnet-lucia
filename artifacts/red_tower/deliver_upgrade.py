"""将通过预览的第二版候选工程保存并重开，交付实际渲染和技术检查记录。"""
import bpy, json, math, time
from pathlib import Path
ROOT=Path(__file__).resolve().parent
OUT=ROOT/'upgrade_v2'
P=json.loads((OUT/'settings.json').read_text())
bpy.ops.wm.open_mainfile(filepath=str(OUT/'candidate.blend'))
SC=bpy.data.scenes['RT_展示'];bpy.context.window.scene=SC
SC.camera=bpy.data.objects['RT_参考透视'];SC.render.resolution_x=int(P['final']*.72);SC.render.resolution_y=P['final']
SC.render.filepath=str(OUT/'beauty_2560.png')
SC['第二版说明']='独立展示灯光、大气、弧形背景和后期；检查场景保留中性灯光'
SC['第二版参数']='upgrade_v2/settings.json；执行 upgrade.py detail、style 后运行 deliver_upgrade.py'
# 默认视口与相机对齐，便于打开后直接检查模型。
for screen in bpy.data.screens:
    for area in screen.areas:
        if area.type=='VIEW_3D':
            area.spaces.active.region_3d.view_perspective='CAMERA'
bpy.ops.wm.save_as_mainfile(filepath=str(ROOT/'red_tower.blend'))
# 真正从磁盘重新读取，后续渲染不依赖保存前的内存状态。
bpy.ops.wm.open_mainfile(filepath=str(ROOT/'red_tower.blend'))
SC=bpy.data.scenes['RT_展示'];bpy.context.window.scene=SC
t=time.time();bpy.ops.render.render(write_still=True)
report={'blender':bpy.app.version_string,'device':'Cycles / Apple M4 Pro Metal','samples':P['samples'],'beauty_resolution':[SC.render.resolution_x,SC.render.resolution_y],'beauty_seconds':round(time.time()-t,2),'reopened_from_disk':True}
comp=SC.compositing_node_group;SC.compositing_node_group=None
SC.render.filepath=str(OUT/'beauty_raw_2560.png');bpy.ops.render.render(write_still=True)
for cam,name in [('中段近景','middle'),('塔顶近景','top'),('塔底近景','base')]:
    SC.camera=bpy.data.objects['RT_'+cam];SC.render.resolution_x=1440;SC.render.resolution_y=1440
    SC.render.filepath=str(OUT/(name+'_lit.png'));bpy.ops.render.render(write_still=True)
# 结构检查关闭展示灯光、大气与背景；新增实体仍保留。
check=bpy.data.scenes['RT_结构检查'];bpy.context.window.scene=check
check.render.resolution_x=864;check.render.resolution_y=1200;check.cycles.samples=32
for cam,name in [('正面','front'),('侧面','side'),('背面','back'),('参考透视','perspective')]:
    check.camera=bpy.data.objects['RT_'+cam];check.render.filepath=str(OUT/('check_'+name+'.png'));bpy.ops.render.render(write_still=True)
report['invalid_meshes']=[];report['zero_area_faces']=[];report['nonfinite']=[];report['missing_images']=[]
seen=set();count=0
for o in bpy.data.objects:
    if not o.name.startswith(('RT_','RT2_')):continue
    count+=1
    if o.type!='MESH' or o.data.name in seen:continue
    seen.add(o.data.name);cp=o.data.copy()
    if cp.validate():report['invalid_meshes'].append(o.name)
    bpy.data.meshes.remove(cp)
    if any(p.area<1e-10 for p in o.data.polygons):report['zero_area_faces'].append(o.name)
    if any(not math.isfinite(v) for p in o.data.vertices for v in p.co):report['nonfinite'].append(o.name)
for im in bpy.data.images:
    if im.source=='FILE' and not im.packed_file and not Path(bpy.path.abspath(im.filepath)).exists():report['missing_images'].append(im.filepath)
report['managed_objects']=count;report['unique_meshes']=len(seen)
report['scenes']=[s.name for s in bpy.data.scenes]
(OUT/'validation.json').write_text(json.dumps(report,ensure_ascii=False,indent=2))
print(json.dumps(report,ensure_ascii=False))
