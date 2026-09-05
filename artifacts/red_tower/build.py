"""红塔可复现制作：仅管理 RT_ 前缀数据；工作高度为假设尺度，正面朝 -Y。"""
import bpy, math, random, json, sys
from pathlib import Path
from mathutils import Vector
ROOT=Path(__file__).resolve().parent
P=json.loads((ROOT/'parameters.json').read_text())
args=sys.argv[sys.argv.index('--')+1:] if '--' in sys.argv else []
STAGE=args[0] if args else 'B'
REV=P['revision']
R=random.Random(P['seed'])
SC=bpy.context.scene
SC.view_layers[0].material_override=None
SC.compositing_node_group=None
for old in list(bpy.data.scenes):
    if old!=SC and old.name.startswith('RT_'):bpy.data.scenes.remove(old)
# 只清理本任务拥有的数据；不触碰其他对象。
for o in list(bpy.data.objects):
    if o.name.startswith('RT_'): bpy.data.objects.remove(o,do_unlink=True)
for c in list(bpy.data.collections):
    if c.name.startswith('RT_'): bpy.data.collections.remove(c)
for bank in (bpy.data.meshes,bpy.data.materials,bpy.data.cameras,bpy.data.lights,bpy.data.node_groups):
    for d in list(bank):
        if d.name.startswith('RT_') and (d.users==0 or (d.use_fake_user and d.users==1)): bank.remove(d)
COL={}
for name in ['骨架','片层','底座','能量','碎片','环境','摄影']:
    c=bpy.data.collections.new('RT_'+name); SC.collection.children.link(c); COL[name]=c
def mat(name,color,metal=.0,rough=.6,emit=0):
    """建立独立可调的材质；发光与曝光互不绑定。"""
    m=bpy.data.materials.new('RT_'+name); m.diffuse_color=(*color,1); m.use_nodes=True
    b=m.node_tree.nodes.get('Principled BSDF')
    b.inputs['Base Color'].default_value=(*color,1)
    b.inputs['Metallic'].default_value=metal;b.inputs['Roughness'].default_value=rough
    b.inputs['Emission Color'].default_value=(*color,1);b.inputs['Emission Strength'].default_value=emit
    return m
M=[mat('氧化钢',(.075,.065,.063),.72,.58),mat('边缘钢',(.17,.155,.14),.7,.42),mat('断裂截面',(.24,.19,.15),.45,.7),mat('残存混凝土',(.22,.215,.20),.03,.88)]
GRAY=mat('检查灰',(.36,.36,.36));GRAY.use_fake_user=True
EM=mat('红色能量',(.8,.008,.003),.1,.3,P['emission'])
TIP=mat('塔顶能量',(1,.025,.008),0,.3,P['tip_emission'])
def obj(name,verts,faces,col,material):
    """创建有明确分组的网格；面绕序由调用方保持外法线。"""
    me=bpy.data.meshes.new('RT_'+name);me.from_pydata(verts,[],faces);me.update()
    o=bpy.data.objects.new('RT_'+name,me);COL[col].objects.link(o);me.materials.append(material)
    return o
CUBE=None
def box(name,pos,size,col='骨架',material=None,rot=0,bevel=0):
    """复用单位盒网格，按构件尺度设置倒角；返回可独立编辑的对象。"""
    global CUBE
    if CUBE is None:
        CUBE=bpy.data.meshes.new('RT_共享梁')
        CUBE.from_pydata([(-.5,-.5,-.5),(.5,-.5,-.5),(.5,.5,-.5),(-.5,.5,-.5),(-.5,-.5,.5),(.5,-.5,.5),(.5,.5,.5),(-.5,.5,.5)],[],[(0,3,2,1),(4,5,6,7),(0,1,5,4),(1,2,6,5),(2,3,7,6),(3,0,4,7)])
        CUBE.materials.append(M[0])
    o=bpy.data.objects.new('RT_'+name,CUBE);COL[col].objects.link(o);o.location=pos;o.scale=size;o.rotation_euler.z=rot
    o.material_slots[0].link='OBJECT';o.material_slots[0].material=material or M[0]
    if bevel:
        b=o.modifiers.new('构件边缘','BEVEL');b.width=bevel;b.segments=2
    return o
def beam(name,a,b,width,depth,col='骨架',material=None):
    """在两个连接点之间放置矩形梁，保持端点支承连续。"""
    a,b=Vector(a),Vector(b)
    o=box(name,(a+b)/2,(width,depth,(b-a).length),col,material)
    o.rotation_euler=(b-a).to_track_quat('Z','Y').to_euler()
    return o
def radius(z):
    """按高度分区线性收分，不把透视缩短烘入模型。"""
    for (a,x),(b,y) in zip(P['profile'],P['profile'][1:]):
        if a<=z<=b:return x+(y-x)*(z-a)/(b-a)
    return .4
def ringpiece(z,r,a,span,depth,thick,material):
    """带切角和厚度的开放多边形片层；保留核心与骨架间的可读空隙。"""
    ri=max(.4,r-depth)
    # 外缘局部凹缺口替代整齐扇形；切口不切断内侧连接。
    radii=[r*.94,r,r-depth*.36,r-depth*.29,r*.96,ri,ri,ri]
    angles=[a,a+span*.27,a+span*.32,a+span*.52,a+span,a+span,a+span*.45,a]
    pts=[(math.cos(t)*rr,math.sin(t)*rr) for rr,t in zip(radii,angles)]
    vs=[(x,y,z+dz) for dz in [-thick/2,thick/2] for x,y in pts]
    n=len(pts);fs=[tuple(reversed(range(n))),tuple(range(n,n*2))]
    fs += [(i,(i+1)%n,(i+1)%n+n,i+n) for i in range(n)]
    return obj('断角片层',vs,fs,'片层',material)
# 连续主骨架：六条脊柱中正面留出能量裂隙。
levels=[0,8,20,38,58,78,94,99]
for j,a in enumerate([-.30,.6,1.65,2.65,3.65,4.4]):
    for lo,hi in zip(levels,levels[1:]):
        rr,ss=radius(lo),radius(hi)
        beam('连续承重脊', (rr*math.cos(a),rr*math.sin(a),lo),(ss*math.cos(a),ss*math.sin(a),hi),max(.18,rr*.15),max(.24,rr*.21))
# 偏心的核心载体与纵向隔板。
for lo,hi in zip(levels,levels[1:]):
    r=radius((lo+hi)/2)
    box('核心暗色载体',(0,.3,(lo+hi)/2),(r*.8,r*.95,hi-lo),material=M[0])
    for x in [-1,1]:
        beam('裂隙护壁',(x*radius(lo)*.4,-radius(lo)*.74,lo),(x*radius(hi)*.4,-radius(hi)*.74,hi),.23,.4,material=M[1])
        beam('纵向能量轨',(x*radius(lo)*.23,-radius(lo)*.83,lo+max(.05,lo==0)),(x*radius(hi)*.23,-radius(hi)*.83,hi),.105,.14,'能量',EM)
# 片层按分区分组：有共同方向，缺失集中在主要破损区。
n=110 if STAGE=='A' else P['layer_count']
for i in range(n):
    rng=random.Random(P['seed']+i*17)
    z=3+i*93/(n-1)+rng.uniform(-.17,.17);r=radius(z)
    for j in range(6):
        a=j*math.tau/6-.10+.08*math.sin(i*.31+j)
        broken=any(lo<z<hi and j==sector for lo,hi,sector in P['damage'])
        if broken or (i%13==0 and j==2):continue
        ext=rng.uniform(.2,1.3)*(1.35 if z<40 else .42)
        if i%9 in (0,1) and j in [0,3]: ext+=rng.uniform(.6,1.7)*(1 if z<60 else .32)
        ringpiece(z,r+ext,a,.92,r*.40+ext,.19 if STAGE=='A' else rng.uniform(.12,.24),M[i%2])
    if i%2==0 or STAGE!='A':
        rr=r*.6
        box('横向能量窗',(rng.uniform(-.22,.22),-r*.88,z+.08),(rr*rng.uniform(.7,1.55),.16,rng.uniform(.065,.13)),'能量',EM,rot=rng.uniform(-.07,.07))
# 不等高的外侧结构肋板增强实体连续性，避开正面核心裂隙。
for k,(a,z,h) in enumerate([(0.4,12,16),(2.6,19,20),(3.5,32,15),(1.6,45,21),(2.8,62,17),(.5,78,14),(3.0,88,11)]):
    rr=radius(z)*.96
    box('纵向残存壳体',(rr*math.cos(a),rr*math.sin(a),z),(.65,.45,h),material=M[0],rot=a)
# 分区断梁，横向轮廓由少量清晰的突出部组织。
for idx,(z,a,ext) in enumerate([(16,1.5,5),(28,4.6,6),(43,1.4,4),(55,4.8,5),(72,1.7,4),(88,4.5,3),(9,0,8),(13,2.9,6),(21,3.1,8),(25,.1,6),(34,0,8),(38,3.2,5),(48,3.1,7),(51,.2,6),(64,2.9,5),(69,0,5),(76,3.1,4),(85,.1,4),(92,3.1,3)]):
    r=radius(z);a+=.15
    v=Vector((math.cos(a),math.sin(a),0))
    beam('外伸主梁',v*r+Vector((0,0,z)),v*(r+ext)+Vector((0,0,z)),.22,.30,material=M[1])
    beam('外伸斜撑',v*r+Vector((0,0,z-1.9)),v*(r+ext*.62)+Vector((0,0,z)),.16,.2)
    if STAGE!='A':
        for dz in [-.5,.45]:
            beam('平行断梁',v*r+Vector((0,0,z+dz)),v*(r+ext*.65)+Vector((0,0,z+dz)),.1,.17)
# 底座为向塔身汇聚的残破扶壁，避免独立建筑式平台。
for j in range(9):
    a=j*math.tau/9+.2;rng=random.Random(700+j)
    foot= rng.uniform(10,15) if REV==1 else rng.uniform(9,13)
    h=rng.uniform(12,23)
    inner=radius(h)
    # 不规则立体楔形，前后两侧通过厚度连接。
    cross=[(foot,0),(foot+.7,0),(foot*.7,h*.24),(foot*.76,h*.29),(inner+1.2,h*.57),(inner+1.7,h*.61),(inner+.5,h*.8),(inner,h),(inner-.4,h*.73),(foot*.55,h*.28),(foot-2,0)]
    vs=[]
    for y in [-.55,.55]:
        for x,z in cross:vs.append((x*math.cos(a)-y*math.sin(a),x*math.sin(a)+y*math.cos(a),z))
    nn=len(cross)
    fs=[tuple(reversed(range(nn))),tuple(range(nn,nn*2))]+[(k,(k+1)%nn,(k+1)%nn+nn,k+nn) for k in range(nn)]
    obj('破损扶壁',vs,fs,'底座',M[0])
    beam('扶壁外缘',(foot*math.cos(a),foot*math.sin(a),.5),(inner*math.cos(a),inner*math.sin(a),h),.3,.4,'底座',M[1])
# 顶部核心与不对称冠部。
box('塔顶核心',(0,-.5,95.5),(.48,.5,3.0),'能量',TIP)
beam('顶部断脊',(-.8,.3,91),(-.5,.15,101.8),.27,.33)
beam('冠部短梁',(-1,0,98.7),(2.2,0,98.7),.19,.26)
beam('顶部斜接',(-1.2,.2,94),(-.5,.15,99.7),.2,.25)
if STAGE!='A':
    # 纵向残破装甲具有厚度和阶梯切口，沿两侧聚集而不覆盖正面核心。
    ar=random.Random(522)
    for k in range(48):
        z=ar.uniform(4,91);h=ar.uniform(1.4,5.5);a=ar.choice([.1,.8,1.8,2.8,3.5,4.1])+ar.uniform(-.15,.15)
        r=radius(z)+.18;w=ar.uniform(.38,.95)*(1.5 if z<30 else 1)
        outline=[(-w*.5,-h*.5),(w*.5,-h*.5),(w*.5,h*.25),(w*.2,h*.3),(w*.35,h*.5),(-w*.4,h*.43)]
        vs=[]
        for rr in [r,r+.16]:
            vs.extend([(rr*math.cos(a)-x*math.sin(a),rr*math.sin(a)+x*math.cos(a),z+zz) for x,zz in outline])
        fs=[tuple(reversed(range(6))),tuple(range(6,12))]+[(i,(i+1)%6,(i+1)%6+6,i+6) for i in range(6)]
        obj('阶梯断口装甲',vs,fs,'骨架',M[0 if k%4 else 2])
    # 中尺度连接与装甲带，完整区保留大面，破损区露出斜撑。
    for i in range(37):
        z=5+i*2.42;r=radius(z)
        for j,a in enumerate([.5,1.6,2.7,3.7]):
            rr=r*.97
            if (i+j)%7 in (0,1):
                beam('露出的跨层斜撑',(rr*math.cos(a),rr*math.sin(a),z),(radius(z+2)*math.cos(a+.48),radius(z+2)*math.sin(a+.48),z+2),.11,.14,material=M[1])
            else:
                box('分段护甲',(rr*math.cos(a),rr*math.sin(a),z+1),(.6 if z<45 else .32,.18,1.65),material=M[0],rot=a+math.pi/2)
    rng=random.Random(819)
    for i in range(80):
        a=rng.uniform(0,math.tau);r=rng.uniform(4.8,14);z=rng.uniform(.1,.8)
        o=box('基底断块',(r*math.cos(a),r*math.sin(a),z),(rng.uniform(.3,1.7),rng.uniform(.4,2),rng.uniform(.25,1.6)),'底座',M[3 if i%3 else 2],rot=a)
        o.rotation_euler.y=rng.uniform(-.4,.4)
        o.data=o.data.copy()
        for v in o.data.vertices:
            v.co.x+=rng.uniform(-.2,.2);v.co.y+=rng.uniform(-.18,.18);v.co.z+=rng.uniform(-.15,.15)
    for z0,a0 in [(31,.0),(61,3.1),(83,.7)]:
        for i in range(7):
            z=z0+rng.uniform(-2,2);r=radius(z)+rng.uniform(1.5,4);a=a0+rng.uniform(-.35,.35)
            box('局部脱落碎片',(r*math.cos(a),r*math.sin(a),z),(rng.uniform(.15,.6),.22,.14),'碎片',M[1],rot=a)
    # 近景连接座：片层根部的夹具与双联螺栓。
    for z in [8,9.4,11,31,32.4,34,60,61.4,90,91.4,94]:
        r=radius(z)
        for s in [-1,1]:
            x=s*r*.46;y=-r*.81
            box('能量轨固定夹',(x,y-.1,z),(.36,.32,.23),material=M[1])
            for dx in [-.1,.1]:
                box('夹具铆钉',(x+dx,y-.28,z),(.055,.07,.08),material=M[2])
# 环境与摄影独立，不向模型混入云雾或二维补绘。
ground=box('展示地面',(0,0,-.6),(400,400,1),'环境',mat('地面',(.125,.13,.14),0,.85))
def camera(name,pos,target,ortho=None):
    """建立固定检查相机；三个正交方向共享画幅、比例与高度。"""
    d=bpy.data.cameras.new('RT_'+name);o=bpy.data.objects.new('RT_'+name,d);COL['摄影'].objects.link(o)
    o.location=pos;o.rotation_euler=(Vector(target)-o.location).to_track_quat('-Z','Y').to_euler()
    d.clip_end=1000
    if ortho:d.type='ORTHO';d.ortho_scale=ortho
    else:d.lens=55
    return o
CAMS={}
CAMS['front']=camera('正面',(0,-180,50),(0,0,50),112)
CAMS['side']=camera('侧面',(180,0,50),(0,0,50),112)
CAMS['back']=camera('背面',(0,180,50),(0,0,50),112)
CAMS['hero']=camera('参考透视',(33,-181,45),(0,0,50))
CAMS['base']=camera('塔底近景',(27,-40,20),(0,0,10),34)
CAMS['middle']=camera('中段近景',(16,-34,37),(0,0,34),20)
CAMS['top']=camera('塔顶近景',(10,-24,97),(0,0,94.5),17)
def light(name,pos,power,size,color):
    """以独立灯对象提供中性可读照明。"""
    d=bpy.data.lights.new('RT_'+name,'AREA');d.energy=power;d.shape='DISK';d.size=size;d.color=color
    o=bpy.data.objects.new('RT_'+name,d);COL['摄影'].objects.link(o);o.location=pos
    o.rotation_euler=(Vector((0,0,45))-o.location).to_track_quat('-Z','Y').to_euler()
light('主光',(-35,-55,95),100000,55,(1,.94,.88))
light('填充',(40,-15,55),65000,45,(.78,.86,1))
light('轮廓',(-10,40,80),130000,35,(1,.9,.8))
SC.world=bpy.data.worlds.new('RT_中性世界');SC.world.use_nodes=True
SC.world.node_tree.nodes['Background'].inputs[0].default_value=(.22,.24,.27,1)
SC.world.node_tree.nodes['Background'].inputs[1].default_value=.45
SC.render.engine='CYCLES';SC.cycles.samples=P['samples'];SC.cycles.use_denoising=True
prefs=bpy.context.preferences.addons['cycles'].preferences;prefs.compute_device_type='METAL';prefs.refresh_devices()
for d in prefs.devices:d.use=d.type=='METAL'
SC.cycles.device='GPU'
SC.render.image_settings.file_format='PNG';SC.render.resolution_percentage=100
SC.view_settings.view_transform='AgX'
SC.render.film_transparent=False
SC['正面定义']='正面为 -Y，Z 轴向上；100 单位仅为制作尺度'
SC['参数文件']='parameters.json'
# 灰模、无辉光、完整展示分别由显式状态控制；不使用雾和景深。
def state(mode):
    """切换结构检查、纯发光和展示；不改变相机或几何。"""
    SC.view_layers[0].material_override=GRAY if mode=='gray' else None
    for m,v in [(EM,P['emission']),(TIP,P['tip_emission'])]:
        m.node_tree.nodes['Principled BSDF'].inputs['Emission Strength'].default_value=0 if mode=='gray' else v
    COL['碎片'].hide_render=mode=='gray'
    tree=bpy.data.node_groups.get('RT_合成') or bpy.data.node_groups.new('RT_合成','CompositorNodeTree')
    SC.compositing_node_group=tree
    tree.nodes.clear()
    if not tree.interface.items_tree: tree.interface.new_socket(name='Image',in_out='OUTPUT',socket_type='NodeSocketColor')
    rl=tree.nodes.new('CompositorNodeRLayers');out=tree.nodes.new('NodeGroupOutput')
    if mode=='beauty':
        g=tree.nodes.new('CompositorNodeGlare')
        g.inputs['Type'].default_value='Fog Glow';g.inputs['Quality'].default_value='High'
        g.inputs['Threshold'].default_value=2;g.inputs['Strength'].default_value=.14;g.inputs['Size'].default_value=.22
        tree.links.new(rl.outputs['Image'],g.inputs['Image']);tree.links.new(g.outputs['Image'],out.inputs['Image'])
    else:tree.links.new(rl.outputs['Image'],out.inputs['Image'])
def render(view,mode,path,res):
    """将实际场景渲染为 PNG；检查视图保持相同照明和曝光。"""
    state(mode);SC.camera=CAMS[view]
    SC.render.resolution_x=int(res*.72) if view in ['front','side','back','hero'] else res
    SC.render.resolution_y=res
    SC.render.filepath=str(ROOT/path);bpy.ops.render.render(write_still=True)
SC.camera=CAMS['hero']
state('gray' if STAGE=='A' else 'beauty')
for area in bpy.context.screen.areas if bpy.context.screen else []:
    if area.type=='VIEW_3D':
        area.spaces.active.region_3d.view_distance=130
        area.spaces.active.region_3d.view_location=(0,0,50)
SC.render.resolution_x=1474;SC.render.resolution_y=2048
bpy.ops.wm.save_as_mainfile(filepath=str(ROOT/(f'stage_A_v{REV}.blend' if STAGE=='A' else 'stage_B.blend')))
if STAGE=='A':
    for view in ['front','side','back','hero']:render(view,'gray',f'A{REV}_{view}.png',P['preview'])
else:
    for view in ['front','side','back','hero']:render(view,'gray',f'B_{view}.png',P['preview'])
    for view in ['base','middle','top']:render(view,'plain',f'{view}.png',P['preview'])
    render('hero','plain','emission_no_glow.png',P['preview'])
    render('hero','beauty','preview.png',P['preview'])
state('gray' if STAGE=='A' else 'beauty');SC.camera=CAMS['hero']
bpy.ops.wm.save_as_mainfile(filepath=str(ROOT/('stage_A_v%d.blend'%REV if STAGE=='A' else 'red_tower.blend')))
