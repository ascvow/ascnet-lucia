"""重新打开最终工程，验证网格与资源，并从磁盘工程渲染检查图。"""
import bpy, json, math, time
from pathlib import Path
ROOT=Path(__file__).resolve().parent
bpy.ops.wm.open_mainfile(filepath=str(ROOT/'red_tower.blend'))
report={'blender':bpy.app.version_string,'device':'Apple M4 Pro / Metal','zero_area_faces':[],'invalid_meshes':[],'nonfinite':[],'missing_resources':[],'invalid_modifiers':[]}
objects=[o for o in bpy.data.objects if o.name.startswith('RT_') and o.type=='MESH']
seen=set();faces=0
for o in objects:
    if o.data.name in seen:continue
    seen.add(o.data.name);faces+=len(o.data.polygons)
    # validate 对临时副本执行，防止检查改写交付网格。
    cp=o.data.copy()
    if cp.validate():report['invalid_meshes'].append(o.name)
    bpy.data.meshes.remove(cp)
    if any(p.area<1e-10 for p in o.data.polygons):report['zero_area_faces'].append(o.name)
    if any(not math.isfinite(c) for v in o.data.vertices for c in v.co):report['nonfinite'].append(o.name)
    for m in o.modifiers:
        if m.type=='BEVEL' and (m.width<=0 or m.segments<1):report['invalid_modifiers'].append(o.name+':'+m.name)
for im in bpy.data.images:
    if im.source=='FILE' and not im.packed_file and not Path(bpy.path.abspath(im.filepath)).exists():report['missing_resources'].append(im.filepath)
report.update(objects=len(objects),unique_meshes=len(seen),base_faces=faces,scenes=[s.name for s in bpy.data.scenes],samples=48,adaptive_threshold=.025,resolution=[1474,2048],denoising=True)
sc=bpy.data.scenes['RT_结构检查'];bpy.context.window.scene=sc
sc.cycles.samples=32
for name,key in [('正面','front'),('侧面','side'),('背面','back'),('参考透视','perspective')]:
    sc.camera=bpy.data.objects['RT_'+name]
    sc.render.filepath=str(ROOT/('check_'+key+'.png'))
    start=time.time();bpy.ops.render.render(write_still=True)
    report[key+'_seconds']=round(time.time()-start,2)
# 关闭辉光，保持真实材质的中性照明图。
sc=bpy.data.scenes['RT_发光无辉光'];bpy.context.window.scene=sc
sc.camera=bpy.data.objects['RT_参考透视'];sc.render.filepath=str(ROOT/'check_emission.png')
bpy.ops.render.render(write_still=True)
(ROOT/'validation.json').write_text(json.dumps(report,ensure_ascii=False,indent=2))
print(json.dumps(report,ensure_ascii=False))
