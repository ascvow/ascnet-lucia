"""红塔第二轮升级：细节与展示分别运行，输入为只读归档，保持复现和独立验收。"""
import bpy, math, random, sys, json, bmesh
from pathlib import Path
from mathutils import Vector
ROOT=Path(__file__).resolve().parent
OUT=ROOT/'upgrade_v2'
P=json.loads((OUT/'settings.json').read_text())
MODE=sys.argv[sys.argv.index('--')+1] if '--' in sys.argv else 'detail'
if MODE not in ['detail','style']:raise ValueError('阶段必须为 detail 或 style')
bpy.ops.wm.open_mainfile(filepath=str(OUT/('source.blend' if MODE=='detail' else 'detail.blend')))
SC=bpy.data.scenes['RT_展示'];bpy.context.window.scene=SC
COL={}
def collection(name):
    """新增集合并链接三个资产场景，保证检查场景也能查看新增实体。"""
    c=bpy.data.collections.new('RT2_'+name)
    for s in bpy.data.scenes:s.collection.children.link(c)
    COL[name]=c
    return c
def material(name,color,metal=.65,rough=.42,emission=0):
    """生成独立材质，发光仅由显式强度控制。"""
    m=bpy.data.materials.new('RT2_'+name);m.use_nodes=True
    b=m.node_tree.nodes.get('Principled BSDF');b.inputs['Base Color'].default_value=(*color,1)
    b.inputs['Metallic'].default_value=metal;b.inputs['Roughness'].default_value=rough
    b.inputs['Emission Color'].default_value=(*color,1);b.inputs['Emission Strength'].default_value=emission
    return m
def mesh(name,verts,faces,col,mat):
    """创建结构网格并重算法线，保留真实厚度和可编辑切口。"""
    me=bpy.data.meshes.new('RT2_'+name);me.from_pydata(verts,[],faces);me.update()
    bm=bmesh.new();bm.from_mesh(me);bmesh.ops.recalc_face_normals(bm,faces=list(bm.faces));bm.to_mesh(me);bm.free()
    o=bpy.data.objects.new('RT2_'+name,me);col.objects.link(o);me.materials.append(mat)
    return o
UNIT=None
def box(name,pos,size,col,mat,rot=0):
    """共享基础盒网格；局部倒角跟随构件比例，避免不必要的网格复制。"""
    global UNIT
    if UNIT is None:
        UNIT=bpy.data.meshes.new('RT2_共享盒')
        UNIT.from_pydata([(-.5,-.5,-.5),(.5,-.5,-.5),(.5,.5,-.5),(-.5,.5,-.5),(-.5,-.5,.5),(.5,-.5,.5),(.5,.5,.5),(-.5,.5,.5)],[],[(0,3,2,1),(4,5,6,7),(0,1,5,4),(1,2,6,5),(2,3,7,6),(3,0,4,7)])
        UNIT.materials.append(mat)
    o=bpy.data.objects.new('RT2_'+name,UNIT);col.objects.link(o);o.location=pos;o.scale=size;o.rotation_euler.z=rot
    o.material_slots[0].link='OBJECT';o.material_slots[0].material=mat
    be=o.modifiers.new('局部边缘倒角','BEVEL');be.width=.025;be.segments=2
    return o
def beam(name,a,b,w,d,col,mat):
    """以两端点创建支撑构件，连接位置由明确结构锚点给定。"""
    a,b=Vector(a),Vector(b);o=box(name,(a+b)/2,(w,d,(b-a).length),col,mat)
    o.rotation_euler=(b-a).to_track_quat('Z','Y').to_euler()
    return o
def cable(name,points,r,col,mat):
    """折线管道使用曲线载体，半径可编辑，细分受控。"""
    cu=bpy.data.curves.new('RT2_'+name,'CURVE');cu.dimensions='3D';cu.bevel_depth=r;cu.bevel_resolution=2
    sp=cu.splines.new('POLY');sp.points.add(len(points)-1)
    for p,v in zip(sp.points,points):p.co=(*v,1)
    o=bpy.data.objects.new('RT2_'+name,cu);col.objects.link(o);cu.materials.append(mat);return o
def rad(z):
    """沿原主体收分插值，新增结构不改变主轴和相机。"""
    profile=json.loads((ROOT/'parameters.json').read_text())['profile']
    for (lo,a),(hi,b) in zip(profile,profile[1:]):
        if lo<=z<=hi:return a+(b-a)*(z-lo)/(hi-lo)
    return 1
def render(name,cam,gray=False,res=1200,scene=SC):
    """实际渲染保存，灰模覆盖材质但保持原检查照明。"""
    bpy.context.window.scene=scene
    scene.camera=bpy.data.objects['RT_'+cam]
    scene.view_layers[0].material_override=bpy.data.materials['RT_检查灰'] if gray else None
    scene.render.resolution_x=int(res*.72) if cam in ['参考透视','正面','侧面','背面'] else res
    scene.render.resolution_y=res;scene.render.filepath=str(OUT/name)
    bpy.ops.render.render(write_still=True)
if MODE=='detail':
    # 保存同相机同照明的旧版近景，供实际几何对比。
    comp=SC.compositing_node_group;SC.compositing_node_group=None
    render('before_middle.png','中段近景',True)
    render('before_top.png','塔顶近景',True)
    steel=bpy.data.materials['RT_氧化钢'];edge=bpy.data.materials['RT_边缘钢'];cut=bpy.data.materials['RT_断裂截面']
    bolts=material('钝化紧固件',(.18,.16,.13),.8,.35)
    core=bpy.data.materials['RT_红色能量']
    detail=collection('连接与维修结构');pipes=collection('管线');crown=collection('塔顶冠部');base=collection('基底残骸')
    rng=random.Random(P['seed'])
    # 三个高度区域的维修结构，保留空白区而非全塔均匀撒细节。
    for z in [5,7.5,12,15,27.5,30,33,35,58,61,63,89,92]:
        r=rad(z)
        for a in [.1,2.9,4.0]:
            p=Vector((r*math.cos(a),r*math.sin(a),z));t=Vector((-math.sin(a),math.cos(a),0))
            box('连接法兰',p,(.65,.23,.55),detail,edge,a+math.pi/2)
            for dx in [-.22,.22]:
                for dz in [-.16,.16]:
                    q=p+t*dx+Vector((math.cos(a)*.16,math.sin(a)*.16,dz))
                    box('法兰铆钉',q,(.08,.08,.08),detail,bolts)
            beam('立面连接肋',p+Vector((0,0,-.95)),p+Vector((0,0,.95)),.10,.18,detail,edge)
            q=p+t*rng.uniform(.5,1.2)
            beam('短构架斜撑',p+Vector((0,0,-.7)),q,.10,.12,detail,steel)
    # 核心两侧的导管束随收分连贯走线，夹具以固定间距锚固。
    for side in [-1,1]:
        for lane in range(3):
            pts=[]
            for z in [2,8,18,30,45,60,75,90,96]:
                r=rad(z);pts.append((side*(r*.50+lane*.13),-r*.91+.09*lane,z))
            cable('纵向复合导管',pts,.042 if lane else .068,pipes,edge)
        for z in range(5,94,5):
            r=rad(z)
            box('管束压板',(side*(r*.5+.13),-r*.91-.08,z),(.47,.14,.12),pipes,bolts)
    # 不对称残片与同位底托组成有支承的中尺度层架。
    for k,z in enumerate([9,12,18,24,28,32,36,46,52,60,63,73,84,90]):
        a=.12 if k%2 else 3.4;r=rad(z);length=rng.uniform(1,2.5)*(1 if z<60 else .55)
        p=Vector((r*math.cos(a),r*math.sin(a),z));v=Vector((math.cos(a),math.sin(a),0));t=Vector((-math.sin(a),math.cos(a),0))
        shape=[(0,-.38),(length*.7,-.38),(length,-.12),(length*.6,.05),(length*.82,.28),(0,.42)]
        verts=[tuple(p+v*x+t*y+Vector((0,0,h))) for h in [-.08,.08] for x,y in shape]
        faces=[tuple(reversed(range(6))),tuple(range(6,12))]+[(i,(i+1)%6,(i+1)%6+6,i+6) for i in range(6)]
        mesh('有厚度断口托板',verts,faces,detail,cut if k%4==0 else steel)
        beam('托板根部加强',p+Vector((0,0,-.9)),p+v*(length*.6),.13,.14,detail,edge)
        for q in [p+v*.3,p+v*.6]:box('托板焊接块',q,(.13,.13,.15),detail,bolts)
    # 塔顶用偏心多层框、竖向爪和连接管取代单一裸露长灯条。
    for j in range(7):
        a=j*math.tau/7+.2;r=1.1+(j%3)*.15;h=95.1+(j%4)*.55
        beam('顶部偏心冠爪',(r*math.cos(a),r*math.sin(a),92.2),((r+.15)*math.cos(a),(r+.15)*math.sin(a),h+.9),.17,.22,crown,edge)
        cable('冠爪引流管',[(r*math.cos(a),r*math.sin(a),93),(.8*math.cos(a),.8*math.sin(a),95.4),(0,-.4,96.2)],.045,crown,core if j%3==0 else cut)
    for z,r in [(93,1.4),(94.8,1.25),(96.2,1.15)]:
        for j in range(8):
            if j==2 and z>94:continue
            a=j*math.tau/8;b=(j+1)*math.tau/8
            beam('八角开放冠框',(r*math.cos(a),r*math.sin(a),z),(r*math.cos(b),r*math.sin(b),z),.15,.20,crown,steel)
    # 底部锚座和断裂钢筋沿扶壁聚集，避免变成独立建筑底盘。
    for j in range(7):
        a=j*math.tau/7+.2;r=9+(j%3)*.6
        p=Vector((r*math.cos(a),r*math.sin(a),.35))
        box('锚固底板',p,(1.7,1.1,.2),base,steel,a)
        for t in [-.5,.5]:
            q=p+Vector((t*math.cos(a),t*math.sin(a),.3))
            box('锚杆螺帽',q,(.15,.15,.3),base,bolts)
        for lane in range(3):
            q=p+Vector((lane*.17,0,0))
            cable('破损裸露钢筋',[q,q+Vector((.2,.1,1.1)),q+Vector((-.4,.25,1.65+lane*.2))],.035,base,cut)
    for cam,key in [('中段近景','middle'),('塔顶近景','top'),('塔底近景','base')]:
        render('detail_'+key+'.png',cam,True)
    SC.view_layers[0].material_override=None;SC.compositing_node_group=comp
    SC.camera=bpy.data.objects['RT_参考透视']
    bpy.ops.wm.save_as_mainfile(filepath=str(OUT/'detail.blend'))
else:
    # 展示灯光独立；检查场景不共享灯数据的修改。
    lights=collection('展示灯光');effects=collection('大气效果')
    for s in bpy.data.scenes:
        if s==SC:continue
        for lc in s.view_layers[0].layer_collection.children:
            if lc.name in [lights.name,effects.name]:lc.exclude=True
    for o in list(bpy.data.collections['RT_摄影'].objects):
        if o.type=='LIGHT':
            cp=o.copy();cp.data=o.data.copy();cp.name='RT2_'+o.name;lights.objects.link(cp)
            cp.data.energy={'RT_主光':P['key_power'],'RT_填充':P['fill_power'],'RT_轮廓':P['rim_power']}[o.name]
            cp.data.color={'RT_主光':(.63,.73,1),'RT_填充':(.75,.62,.53),'RT_轮廓':(.45,.59,1)}[o.name]
    # 只在展示场景排除旧灯，用独立中性摄影集合保留其他场景。
    oldlights=bpy.data.collections.new('RT2_旧版中性灯');SC.collection.children.link(oldlights)
    for o in list(bpy.data.collections['RT_摄影'].objects):
        if o.type=='LIGHT':
            oldlights.objects.link(o);bpy.data.collections['RT_摄影'].objects.unlink(o)
    for s in bpy.data.scenes:
        if s!=SC:s.collection.children.link(oldlights)
    SC.view_layers[0].layer_collection.children[oldlights.name].exclude=True
    def lamp(name,pos,power,color,size):
        """局部能量补光具有有限半径，照亮相邻钢架而不是染红整幅画面。"""
        d=bpy.data.lights.new('RT2_'+name,'POINT');d.energy=power;d.color=color;d.shadow_soft_size=size
        o=bpy.data.objects.new('RT2_'+name,d);lights.objects.link(o);o.location=pos
    for z in [9,23,37,51,65,79,91]:
        lamp('核心红色反照',(0,-rad(z)-.5,z),P['red_bounce_power'],(1,.015,.003),1.4)
    lamp('塔顶焦点',(0,-1.2,96),P['tip_light_power'],(1,.025,.005),1.0)
    # 通过天空着色提供云层色调变化；它是程序化世界材质，不是二维补绘。
    SC.world=SC.world.copy();nt=SC.world.node_tree;bg=nt.nodes.get('Background')
    bg.inputs['Strength'].default_value=P['world_strength']
    tex=nt.nodes.new('ShaderNodeTexCoord');noise=nt.nodes.new('ShaderNodeTexNoise')
    noise.inputs['Scale'].default_value=3.8;noise.inputs['Detail'].default_value=4;noise.inputs['Roughness'].default_value=.7
    nt.links.new(tex.outputs['Normal'],noise.inputs['Vector'])
    ramp=nt.nodes.new('ShaderNodeValToRGB');ramp.color_ramp.elements[0].position=.3;ramp.color_ramp.elements[0].color=(.016,.023,.045,1)
    ramp.color_ramp.elements[1].position=.72;ramp.color_ramp.elements[1].color=(.15,.085,.08,1)
    nt.links.new(noise.outputs['Fac'],ramp.inputs[0]);nt.links.new(ramp.outputs[0],bg.inputs[0])
    # 地面扩展只是消除有限平面的硬地平线，不改变本体和相机。
    ground=bpy.data.objects['RT_展示地面']
    # 连续弧形背景承接地面，消除地面与世界背景之间的硬分界。
    backdrop=collection('展示弧形背景')
    for s in bpy.data.scenes:
        if s!=SC:s.view_layers[0].layer_collection.children[backdrop.name].exclude=True
    yz=[(35+60*math.sin(i*math.pi/48),-.10+60*(1-math.cos(i*math.pi/48))) for i in range(25)]
    yz.append((95,220))
    verts=[(x,y,z) for x in [-800,800] for y,z in yz]
    n=len(yz);faces=[(i,i+1,i+1+n,i+n) for i in range(n-1)]
    wall=mesh('无缝弧形背景',verts,faces,backdrop,ground.material_slots[0].material)
    for p in wall.data.polygons:p.use_smooth=True
    haze=material('稀薄背景大气',(.1,.1,.1));nt=haze.node_tree;nt.nodes.clear()
    out=nt.nodes.new('ShaderNodeOutputMaterial');vol=nt.nodes.new('ShaderNodeVolumePrincipled')
    vol.inputs['Density'].default_value=P['haze_density'];vol.inputs['Color'].default_value=(.33,.39,.52,1);vol.inputs['Anisotropy'].default_value=.2
    nt.links.new(vol.outputs['Volume'],out.inputs['Volume'])
    box('稀薄大气域',(0,35,60),(180,130,135),effects,haze)
    for name,val in [('RT_红色能量',P['core_emission']),('RT_塔顶能量',P['tip_emission'])]:
        bpy.data.materials[name].node_tree.nodes.get('Principled BSDF').inputs['Emission Strength'].default_value=val
    tree=bpy.data.node_groups.new('RT2_展示后期','CompositorNodeTree')
    tree.interface.new_socket(name='Image',in_out='OUTPUT',socket_type='NodeSocketColor')
    rl=tree.nodes.new('CompositorNodeRLayers');out=tree.nodes.new('NodeGroupOutput')
    glow=tree.nodes.new('CompositorNodeGlare');glow.label='可独立关闭的柔和辉光'
    for name,val in [('Type','Fog Glow'),('Quality','High'),('Threshold',2.0),('Strength',P['bloom']),('Size',.35)]:glow.inputs[name].default_value=val
    streak=tree.nodes.new('CompositorNodeGlare');streak.label='塔顶光学星芒'
    for name,val in [('Type','Streaks'),('Quality','High'),('Threshold',6.0),('Strength',P['streak']),('Streaks',4),('Streaks Angle',0.0),('Fade',.88)]:streak.inputs[name].default_value=val
    tree.links.new(rl.outputs['Image'],glow.inputs['Image']);tree.links.new(glow.outputs['Image'],streak.inputs['Image']);tree.links.new(streak.outputs['Image'],out.inputs['Image'])
    SC.compositing_node_group=tree;SC.cycles.samples=P['samples'];SC.cycles.adaptive_threshold=.018
    SC.view_settings.exposure=0
    render('style_preview.png','参考透视',False,P['preview'])
    SC.compositing_node_group=None
    render('style_raw.png','参考透视',False,P['preview'])
    SC.compositing_node_group=tree
    SC.camera=bpy.data.objects['RT_参考透视']
    bpy.ops.wm.save_as_mainfile(filepath=str(OUT/'candidate.blend'))
