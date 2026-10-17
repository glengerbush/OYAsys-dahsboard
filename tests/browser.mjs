// Browser journeys use the actual executable and HTTP/storage seam, never a mock API.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, readFile, mkdir, rename, unlink, symlink, copyFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { chromium } from 'playwright';

const now = 2000000000000;
const metrics = [
  {id:'gpu.p100.temperature',label:'P100 temperature',unit:'°C',group:'gpu',source:'NVIDIA Tesla P100 · synthetic'},
  {id:'gpu.p100.utilization',label:'P100 utilization',unit:'%',group:'gpu',source:'NVIDIA Tesla P100 · synthetic'},
  {id:'gpu.p100.power',label:'P100 power',unit:'W',group:'gpu',source:'NVIDIA Tesla P100 · synthetic'},
  {id:'fan.fixture.rpm',label:'Chassis fan 2',unit:'RPM',group:'fan',source:'Synthetic motherboard controller'},
  {id:'fan.missing.rpm',label:'Disconnected fan',unit:'RPM',group:'fan',source:'Synthetic motherboard controller'},
  {id:'cpu.utilization',label:'CPU utilization',unit:'%',group:'cpu',source:'Synthetic CPU'},
  {id:'memory.used',label:'RAM used',unit:'bytes',group:'memory',source:'Synthetic memory'},
  {id:'storage.used',label:'Root filesystem used',unit:'bytes',group:'storage',source:'Synthetic disk'},
];

async function freePort() {
  const socket = createServer();
  await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  return port;
}
async function server(t, catalog = metrics, cadence = 10000, options = {}) {
  const directory = await mkdtemp(join(tmpdir(), 'oyasys-browser-'));
  const port = await freePort();
  const url = `http://127.0.0.1:${port}`;
  const samples = Array.from({length:120}, (_,index)=>({timestamp_ms:now-(119-index)*cadence,values:{'gpu.p100.temperature':index===60?81:38+index%10,'gpu.p100.utilization':index%50,'gpu.p100.power':31+index%10,'fan.fixture.rpm':1400+index%20*10,'fan.missing.rpm':null,'cpu.utilization':0,'memory.used':8*1024**3,'storage.used':128*1024**3,...options.values}}));
  await writeFile(join(directory,'fixture.json'),JSON.stringify({inventory:{hostname:'minisforum-fixture',system:[{label:'System',value:'Synthetic Minisforum fixture'},{label:'Memory',value:'32 GiB'}],metrics:catalog,warnings:[],synthetic:true,...(options.availability?{availability:options.availability}:{})},samples,now_ms:now}));
  await writeFile(join(directory,'clock'),String(now));
  if(options.blockStorage)await writeFile(join(directory,'data'),'Deliberately unavailable storage for browser failure test');
  const updater=join(directory,'updater');
  if(options.updater){await mkdir(join(updater,'inbox'),{recursive:true});await writeUpdateStatus(updater,{state:'idle',message:'Check for a new release when needed.'});}
  let logs='';let child;let currentBinary=resolve(process.env.OYASYS_BINARY||'target/debug/oyasys');
  const launch=()=>{
    child=spawn(currentBinary,['serve','--bind',`127.0.0.1:${port}`,'--origin',url,'--data-dir',join(directory,'data'),'--fixture',join(directory,'fixture.json'),'--fixture-clock',join(directory,'clock')],{stdio:['ignore','ignore','pipe'],env:{...process.env,OYASYS_UPDATER_DIR:options.updater?updater:''}});
    child.stderr.on('data',chunk=>logs+=chunk);
  };
  const stop=async()=>{if(child.exitCode!==null||child.signalCode!==null)return;child.kill('SIGTERM');const force=setTimeout(()=>child.kill('SIGKILL'),1500);await new Promise(resolve=>child.exitCode!==null||child.signalCode!==null?resolve():child.once('exit',resolve));clearTimeout(force);};
  const start=async(binary)=>{
    if(binary)currentBinary=resolve(binary);
    launch();
    for(let attempt=0;attempt<100;attempt++){
      if(child.exitCode!==null||child.signalCode!==null)throw new Error(`Server exited: ${logs}`);
      try{if((await fetch(`${url}/api/v1/state`)).ok)return;}catch{}
      await new Promise(resolve=>setTimeout(resolve,50));
    }
    throw new Error(`Server did not listen: ${logs}`);
  };
  t.after(async()=>{await stop();await rm(directory,{recursive:true,force:true});});
  await start();return {url,directory,updater,stop,start};
}
// The fake privileged worker communicates through the actual bridge files.
// Browser fetch and the OYAsys HTTP process are never mocked.
async function writeUpdateStatus(directory,status) {
  const data={state:'idle',latest_version:null,message:'',checked_at:null,request_id:null,...status};
  await writeFile(join(directory,'status.next.json'),JSON.stringify(data));
  await rename(join(directory,'status.next.json'),join(directory,'status.json'));
}
async function completeUpdateRequest(directory,status) {
  const request=JSON.parse(await readFile(join(directory,'inbox','request.json'),'utf8'));
  await writeUpdateStatus(directory,{...status,request_id:request.id});
  await unlink(join(directory,'inbox','request.json'));
  return request;
}
let candidateBuild;
async function candidateBinary() {
  if(!candidateBuild)candidateBuild=(async()=>{
    const directory=await mkdtemp(join(tmpdir(),'oyasys-version-fixture-'));const target=resolve('target');
    try{
      const original=await readFile(resolve('Cargo.toml'),'utf8');const version=original.match(/^version = "(\d+)\.(\d+)\.(\d+)"/m);
      assert.ok(version,'Candidate test expects the normal semantic package version');
      const candidateVersion=`${version[1]}.${version[2]}.${Number(version[3])+1}`;
      const manifest=original.replace('[package]','[package]\nautobins = false').replace(/^version = "[^"\n]+"/m,`version = "${candidateVersion}"`)+ '\n[[bin]]\nname = "oyasys-browser-candidate"\npath = "src/main.rs"\n';
      await writeFile(join(directory,'Cargo.toml'),manifest);await copyFile(resolve('Cargo.lock'),join(directory,'Cargo.lock'));
      for(const entry of ['src','web','build.rs'])await symlink(resolve(entry),join(directory,entry));
      await new Promise((resolveBuild,reject)=>{
        const build=spawn('cargo',['build','--offline','--manifest-path',join(directory,'Cargo.toml'),'--target-dir',target,'--bin','oyasys-browser-candidate'],{stdio:['ignore','pipe','pipe']});let output='';
        build.stdout.on('data',chunk=>output+=chunk);build.stderr.on('data',chunk=>output+=chunk);build.once('error',reject);
        build.once('exit',code=>code===0?resolveBuild():reject(new Error(`Candidate build failed: ${output}`)));
      });
      return {binary:join(target,'debug','oyasys-browser-candidate'),version:candidateVersion};
    }finally{await rm(directory,{recursive:true,force:true});}
  })();
  return candidateBuild;
}
async function browser(t) {
  const instance=await chromium.launch({executablePath:process.env.CHROMIUM_PATH||'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
  t.after(()=>instance.close());
  return instance;
}

test('dashboard shows real HTTP fixture, ranges, verified mapping, settings and persisted reload', {timeout:60000}, async t=>{
  const {url}=await server(t);const instance=await browser(t);const page=await instance.newPage({viewport:{width:1440,height:1050}});page.setDefaultTimeout(10000);
  const errors=[];page.on('pageerror',error=>errors.push(error.message));
  await page.goto(url);
  await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();
  await page.getByRole('heading',{name:'P100 temperature · °C',exact:true}).waitFor();
  await page.getByRole('heading',{name:'Cooling fan not verified',exact:true}).waitFor();
  await page.waitForFunction(()=>document.querySelectorAll('canvas').length>0);
  await page.getByText(/^Sources & availability/).click();
  assert.match(await page.getByRole('row').filter({hasText:'CPU utilization'}).last().innerText(),/0%/);
  assert.match(await page.getByRole('row').filter({hasText:'Disconnected fan'}).last().innerText(),/Unavailable/);
  await page.getByLabel('Time range',{exact:true}).selectOption('21600000');
  await page.getByRole('button',{name:'Settings',exact:true}).click();
  await page.getByLabel('Sampling interval seconds',{exact:true}).fill('2');
  await page.getByLabel('Full-detail retention days',{exact:true}).fill('20');
  await page.getByLabel('I understand that shortening retention deletes older readings.').check();
  await page.getByLabel('P100 cooling fan',{exact:true}).selectOption('fan.fixture.rpm');
  await page.getByRole('button',{name:'Save settings',exact:true}).click();
  await page.getByText('Settings saved. Collection uses the new settings.').waitFor();
  await page.getByRole('button',{name:'Close settings',exact:true}).click();
  await page.getByRole('heading',{name:'P100 cooling fan · RPM',exact:true}).waitFor();
  await page.reload();
  await page.getByRole('heading',{name:'P100 cooling fan · RPM',exact:true}).waitFor();
  await page.getByRole('button',{name:'Settings',exact:true}).click();
  assert.equal(await page.getByLabel('Sampling interval seconds',{exact:true}).inputValue(),'2');
  assert.equal(await page.getByLabel('Full-detail retention days',{exact:true}).inputValue(),'20');
  await page.keyboard.press('Escape');
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true,'phone page should not overflow horizontally');
  await page.getByLabel('Time range',{exact:true}).selectOption('custom');
  await page.getByRole('button',{name:'Apply range',exact:true}).click();
  await page.getByRole('button',{name:'Refresh dashboard',exact:true}).click();
  assert.deepEqual(errors,[]);
});

test('filesystem capacity distinguishes used, available, reserved, empty and unavailable readings', {timeout:30000}, async t=>{
  const catalog=[];const values={};const GiB=1024**3;
  for(const [key,path,total,used,available] of [['root','/',512*GiB,300*GiB,196*GiB],['data','/data',1024*GiB,0,1024*GiB],['missing','/offline',null,null,null]]){
    for(const [suffix,label,value] of [['total','capacity',total],['used','used',used],['available','available',available]]){
      const id=`filesystem.${key}.${suffix}`;catalog.push({id,label:`${path} ${label}`,unit:'bytes',group:'storage',source:`ext4 · Synthetic disk · ${path}`});values[id]=value;
    }
  }
  const {url}=await server(t,catalog,10000,{values});const instance=await browser(t);const page=await instance.newPage({viewport:{width:1440,height:1000}});const errors=[];page.on('pageerror',error=>errors.push(error.message));await page.goto(url);
  const root=page.locator('.filesystem-card').filter({has:page.getByRole('heading',{name:'/',exact:true})});await root.getByText('196 GiB available',{exact:true}).waitFor();
  assert.match(await root.innerText(),/300 GiB · 58.6%/);assert.match(await root.innerText(),/16 GiB · 3.1%/);
  const empty=page.locator('.filesystem-card').filter({has:page.getByRole('heading',{name:'/data',exact:true})});assert.match(await empty.innerText(),/1 TiB available/);assert.match(await empty.innerText(),/0 B · 0%/);
  const unavailable=page.locator('.filesystem-card').filter({has:page.getByRole('heading',{name:'/offline',exact:true})});assert.match(await unavailable.innerText(),/Unavailable/);assert.equal(await unavailable.locator('.filesystem-bar').isVisible(),false);assert.equal(await unavailable.getByRole('button',{name:'Analyze folders'}).isDisabled(),true);
  await page.setViewportSize({width:390,height:844});assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);assert.deepEqual(errors,[]);
});

test('folder breakdown scans only on click and renders real file sizes safely', {timeout:30000}, async t=>{
  const mount=await mkdtemp(join(tmpdir(),'oyasys-folders-'));t.after(()=>rm(mount,{recursive:true,force:true}));
  await mkdir(join(mount,'Models <P100>'));await writeFile(join(mount,'Models <P100>','weights.bin'),Buffer.alloc(2*1024**2,7));await mkdir(join(mount,'Logs'));await writeFile(join(mount,'Logs','server.log'),Buffer.alloc(1024**2,8));
  const catalog=['total','used','available'].map(suffix=>({id:`filesystem.test.${suffix}`,label:`${mount} ${suffix==='total'?'capacity':suffix}`,unit:'bytes',group:'storage',source:'Synthetic filesystem'}));
  const {url}=await server(t,catalog,10000,{values:{'filesystem.test.total':10*1024**3,'filesystem.test.used':2*1024**3,'filesystem.test.available':8*1024**3}});
  const instance=await browser(t);const page=await instance.newPage();const requests=[];page.on('request',request=>{if(request.url().endsWith('/storage/analyze'))requests.push(request.method());});await page.goto(url);await page.getByText('8 GiB available',{exact:true}).waitFor();assert.deepEqual(requests,[]);
  await page.getByRole('button',{name:'Analyze folders',exact:true}).click();await page.getByRole('heading',{name:'Folder breakdown',exact:true}).waitFor();assert.deepEqual(requests,['POST']);
  const analysis=page.getByRole('region',{name:`Folder breakdown for ${mount}`,exact:true});assert.match(await analysis.innerText(),/Models <P100>/);assert.match(await analysis.innerText(),/2 MiB/);assert.equal(await analysis.locator('p100').count(),0);assert.match(await analysis.innerText(),/Logs/);assert.match(await analysis.innerText(),/Scanned/);
  assert.equal(await page.getByRole('button',{name:'Analyze folders',exact:true}).isEnabled(),true);
});

test('absence of P100 remains explicit; available CPU zero remains a reading', {timeout:30000}, async t=>{
  const {url}=await server(t,metrics.filter(metric=>metric.group==='cpu'));
  const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);
  await page.goto(url);
  await page.getByRole('heading',{name:'P100 telemetry unavailable',exact:true}).waitFor();
  await page.getByText(/^Sources & availability/).click();
  assert.match(await page.getByRole('row').filter({hasText:'CPU utilization'}).last().innerText(),/0%/);
});

test('steady visible cadence uses one updates request; hidden document pauses and resumes', {timeout:30000}, async t=>{
  const {url}=await server(t);
  const initial=await (await fetch(`${url}/api/v1/state`)).json();
  const saved=await fetch(`${url}/api/v1/settings`,{method:'PUT',headers:{'Content-Type':'application/json',Origin:url,'X-OYAsys-CSRF':initial.csrf_token},body:JSON.stringify({...initial.settings,sampling_seconds:1,confirm_delete:false})});
  assert.equal(saved.status,200);
  const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);
  await page.goto(url);await page.waitForRequest(request=>request.url().includes('/api/v1/updates?'));
  const requests=[];page.on('request',request=>{if(request.url().includes('/api/v1/'))requests.push(new URL(request.url()).pathname);});
  await page.waitForTimeout(1200);
  assert.ok(requests.length>=1&&requests.length<=2,`Unexpected cadence: ${requests}`);
  assert.ok(requests.every(path=>path==='/api/v1/updates'),`Polling must batch state and samples: ${requests}`);
  // Headless Chromium keeps all test tabs visible. Simulate only the browser's
  // public visibility boundary; app internals and HTTP responses remain real.
  await page.evaluate(()=>{Object.defineProperty(document,'hidden',{configurable:true,value:true});document.dispatchEvent(new Event('visibilitychange'));});
  await page.waitForTimeout(100);requests.length=0;
  await page.waitForTimeout(2200);assert.deepEqual(requests,[],'Hidden documents must not poll');
  const resumed=page.waitForRequest(request=>request.url().includes('/api/v1/updates?'));
  await page.evaluate(()=>{delete document.hidden;document.dispatchEvent(new Event('visibilitychange'));});
  await resumed;
});

test('any available source can replace one of the first 64 charted sensors', {timeout:30000}, async t=>{
  const catalog=Array.from({length:70},(_,index)=>({id:`thermal.extra${index}`,label:`Extra temperature ${index}`,unit:'°C',group:'thermal',source:'Synthetic expanded catalog'}));
  const {url}=await server(t,catalog);const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);
  await page.goto(url);await page.getByText(/^Sources & availability/).click();
  await page.getByRole('checkbox',{name:'Chart Extra temperature 0 (thermal.extra0)',exact:true}).uncheck();
  await page.getByRole('checkbox',{name:'Chart Extra temperature 69 (thermal.extra69)',exact:true}).check();
  const history=page.waitForRequest(request=>request.url().includes('/api/v1/history?'));
  await page.getByRole('button',{name:'Apply chart sources',exact:true}).click();
  const requested=new URL((await history).url()).searchParams.get('metrics').split(',');
  assert.equal(requested.length,64);assert.ok(requested.includes('thermal.extra69'));assert.ok(!requested.includes('thermal.extra0'));
  await page.getByRole('checkbox',{name:'Chart Extra temperature 68 (thermal.extra68)',exact:true}).click();
  await page.getByText('64 chart sources maximum. Uncheck another source first.').waitFor();
  assert.equal(await page.getByRole('checkbox',{name:'Chart Extra temperature 68 (thermal.extra68)',exact:true}).isChecked(),false);
});


test('isolated readings remain visible across collection gaps', {timeout:30000}, async t=>{
  const {url}=await server(t,metrics,60000);const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);
  await page.goto(url);
  const panel=page.getByRole('article').filter({has:page.getByRole('heading',{name:'P100 temperature · °C',exact:true})});
  const canvas=panel.locator('canvas');await canvas.waitFor();
  const coloredPixels=await canvas.evaluate(canvas=>{
    const pixels=canvas.getContext('2d').getImageData(0,0,canvas.width,canvas.height).data;let count=0;
    for(let index=0;index<pixels.length;index+=4)if(pixels[index]<160&&pixels[index+1]>150&&pixels[index+2]>190&&pixels[index+3]>100)count++;
    return count;
  });
  assert.ok(coloredPixels>15,`Isolated readings must draw visible cyan marks; found ${coloredPixels} pixels`);
  await panel.getByText('Reading summary',{exact:true}).click();
  assert.match(await panel.getByRole('table').innerText(),/°C/);
});

test('settings contains keyboard focus and Escape restores its trigger on phones', {timeout:30000}, async t=>{
  const {url}=await server(t);const instance=await browser(t);const page=await instance.newPage({viewport:{width:390,height:844}});page.setDefaultTimeout(10000);
  await page.goto(url);await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();
  const trigger=page.getByRole('button',{name:'Settings',exact:true});await trigger.click();
  const dialog=page.getByRole('dialog',{name:'Dashboard settings',exact:true});await dialog.waitFor();
  await dialog.getByRole('button',{name:'Save settings',exact:true}).focus();await page.keyboard.press('Tab');
  assert.equal(await dialog.getByRole('button',{name:'Close settings',exact:true}).evaluate(button=>button===document.activeElement),true);
  await page.keyboard.press('Shift+Tab');
  assert.equal(await dialog.getByRole('button',{name:'Save settings',exact:true}).evaluate(button=>button===document.activeElement),true);
  for(let index=0;index<10;index++){
    await page.keyboard.press('Tab');assert.equal(await dialog.evaluate(dialog=>dialog.contains(document.activeElement)),true,'Tab must stay in settings');
  }
  await page.keyboard.press('Escape');assert.equal(await dialog.isVisible(),false);
  assert.equal(await trigger.evaluate(button=>button===document.activeElement),true);
});


test('unavailable history falls back to live state without reporting a disconnected server', {timeout:30000}, async t=>{
  const {url}=await server(t,metrics,10000,{blockStorage:true});const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);
  await page.clock.install();await page.goto(url);
  await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();
  await page.getByText(/^History unavailable:/).first().waitFor();
  const failedUpdate=page.waitForResponse(response=>response.url().includes('/api/v1/updates?')&&response.status()===503);
  const liveFallback=page.waitForResponse(response=>response.url().endsWith('/api/v1/state')&&response.status()===200);
  await page.clock.runFor(11000);await failedUpdate;await liveFallback;
  await page.getByText(/History updates unavailable:.*Live readings are still available/).waitFor();
  assert.equal(await page.getByText('Disconnected',{exact:true}).count(),0);
  const p100=page.getByRole('article').filter({has:page.getByRole('heading',{name:'P100 temperature · °C',exact:true})});
  assert.match(await p100.innerText(),/47 °C/);
});

test('null sources explain availability status and reason', {timeout:30000}, async t=>{
  const {url}=await server(t,metrics,10000,{availability:{'fan.missing.rpm':{status:'permission_denied',reason:'Read access denied for the motherboard fan sensor'}}});
  const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);await page.goto(url);
  await page.getByText(/^Sources & availability/).click();
  const source=page.getByRole('row').filter({hasText:'Disconnected fan'});
  assert.match(await source.innerText(),/Permission denied · Read access denied for the motherboard fan sensor/);
});


test('software updates stay dormant until Settings opens and explain disabled setup', {timeout:30000}, async t=>{
  const {url}=await server(t);const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);
  const requests=[];page.on('request',request=>{if(request.url().includes('/software-update'))requests.push(request.url());});
  await page.goto(url);await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();
  assert.deepEqual(requests,[]);
  await page.getByRole('button',{name:'Settings',exact:true}).click();
  await page.getByText('Dashboard updates need one-time setup on the server. Enable them using the installation guide.').waitFor();
  assert.equal(await page.getByRole('button',{name:'Check for updates',exact:true}).isDisabled(),true);
  assert.equal(requests.length,1);assert.ok(requests[0].endsWith('/software-update'));
});

test('software check and install use the bridge, preserve edits, reconnect and reload after update', {timeout:45000}, async t=>{
  const {url,updater,stop,start}=await server(t,metrics,10000,{updater:true});const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);await page.clock.install();
  await page.goto(url);await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();
  await page.getByRole('button',{name:'Settings',exact:true}).click();
  const checking=page.waitForResponse(response=>response.url().endsWith('/software-update/check')&&response.status()===202);
  await page.getByRole('button',{name:'Check for updates',exact:true}).click();await checking;
  const check=await completeUpdateRequest(updater,{state:'available',latest_version:'9.9.9',message:'A new release is available.',checked_at:2000000000});assert.equal(check.action,'check');
  await page.clock.runFor(2200);await page.getByRole('button',{name:'Update to 9.9.9',exact:true}).waitFor();
  await page.getByLabel('Sampling interval seconds',{exact:true}).fill('20');
  assert.equal(await page.getByRole('button',{name:'Update to 9.9.9',exact:true}).isDisabled(),true);
  await page.getByText('Save or cancel your settings changes before updating.').waitFor();
  await page.getByRole('button',{name:'Cancel',exact:true}).click();await page.getByRole('button',{name:'Settings',exact:true}).click();
  assert.equal(await page.getByLabel('Sampling interval seconds',{exact:true}).inputValue(),'10');
  const installing=page.waitForResponse(response=>response.url().endsWith('/software-update/install')&&response.status()===202);
  await page.getByRole('button',{name:'Update to 9.9.9',exact:true}).click();await installing;
  assert.equal(await page.getByLabel('Sampling interval seconds',{exact:true}).isDisabled(),true);
  const install=JSON.parse(await readFile(join(updater,'inbox','request.json'),'utf8'));assert.equal(install.action,'install');assert.equal(install.version,'9.9.9');
  await stop();await page.clock.runFor(2200);await page.getByText('Restarting OYAsys… Reconnecting to the updated service.').waitFor();
  await completeUpdateRequest(updater,{state:'updated',latest_version:'9.9.9',message:'Update completed.',checked_at:2000000000});await start();
  const reload=page.waitForEvent('load');await page.clock.runFor(2200);await reload;
  await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();
  assert.equal(await page.getByRole('dialog',{name:'Dashboard settings',exact:true}).isVisible(),false);
});

test('software polling stops closed, hidden and terminal; errors remain actionable', {timeout:45000}, async t=>{
  const {url,updater}=await server(t,metrics,10000,{updater:true});const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);await page.clock.install();
  const requests=[];page.on('request',request=>{if(request.url().endsWith('/software-update'))requests.push(request.url());});
  await page.goto(url);await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();await page.getByRole('button',{name:'Settings',exact:true}).click();
  const checking=page.waitForResponse(response=>response.url().endsWith('/software-update/check')&&response.status()===202);
  await page.getByRole('button',{name:'Check for updates',exact:true}).click();await checking;
  await page.getByRole('button',{name:'Close settings',exact:true}).click();requests.length=0;await page.clock.runFor(6500);assert.equal(requests.length,0);
  await page.getByRole('button',{name:'Settings',exact:true}).click();await page.getByRole('button',{name:'Checking…',exact:true}).waitFor();
  await page.evaluate(()=>{Object.defineProperty(document,'hidden',{configurable:true,value:true});document.dispatchEvent(new Event('visibilitychange'));});requests.length=0;
  await page.clock.runFor(6500);assert.equal(requests.length,0);
  await page.evaluate(()=>{delete document.hidden;document.dispatchEvent(new Event('visibilitychange'));});
  const outcome=await completeUpdateRequest(updater,{state:'error',message:'The release server could not be reached.'});assert.equal(outcome.action,'check');
  await page.clock.runFor(2200);await page.getByText('The release server could not be reached.',{exact:true}).waitFor();
  assert.equal(await page.getByRole('button',{name:'Check for updates',exact:true}).isEnabled(),true);
  requests.length=0;await page.clock.runFor(6500);assert.equal(requests.length,0);
});


test('software candidate version keeps polling until verified success, including another client install', {timeout:120000}, async t=>{
  const candidate=await candidateBinary();const {url,updater,stop,start}=await server(t,metrics,10000,{updater:true});
  const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);await page.clock.install();
  await page.goto(url);await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();await page.getByRole('button',{name:'Settings',exact:true}).click();
  await page.getByText('Check for a new release when needed.',{exact:true}).waitFor();
  // Another client starts an install. Opening Settings observes it through real HTTP.
  await page.getByRole('button',{name:'Close settings',exact:true}).click();
  const requestId='other-client-install';await writeUpdateStatus(updater,{state:'installing',latest_version:candidate.version,request_id:requestId,message:'Candidate health checks are running.'});
  await page.getByRole('button',{name:'Settings',exact:true}).click();await page.getByText('Candidate health checks are running.',{exact:true}).waitFor();
  let navigations=0;page.on('framenavigated',frame=>{if(frame===page.mainFrame())navigations++;});
  await stop();await start(candidate.binary);await page.clock.runFor(2200);
  await page.getByText(`Version ${candidate.version}`,{exact:true}).waitFor();
  assert.equal(navigations,0,'Serving a candidate version must not cause an early navigation');
  assert.equal(await page.getByRole('dialog',{name:'Dashboard settings',exact:true}).isVisible(),true);
  assert.equal(await page.getByRole('button',{name:'Reload dashboard',exact:true}).isVisible(),false);
  const polling=page.waitForResponse(response=>response.url().endsWith('/software-update'));await page.clock.runFor(2200);await polling;
  await writeUpdateStatus(updater,{state:'updated',latest_version:candidate.version,request_id:requestId,message:'Candidate verified; update committed.'});
  const reloaded=page.waitForEvent('load');await page.clock.runFor(2200);await reloaded;
  assert.equal(navigations,1,'Verified successful update must reload the assets exactly once');
});

test('software candidate rollback stays visible instead of silently navigating away', {timeout:120000}, async t=>{
  const candidate=await candidateBinary();const originalBinary=resolve(process.env.OYASYS_BINARY||'target/debug/oyasys');
  const {url,updater,stop,start}=await server(t,metrics,10000,{updater:true});const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);await page.clock.install();
  await writeUpdateStatus(updater,{state:'available',latest_version:candidate.version,message:'A new release is available.'});
  await page.goto(url);await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();await page.getByRole('button',{name:'Settings',exact:true}).click();
  const accepted=page.waitForResponse(response=>response.url().endsWith('/software-update/install')&&response.status()===202);
  await page.getByRole('button',{name:`Update to ${candidate.version}`,exact:true}).click();await accepted;
  await stop();await start(candidate.binary);let navigations=0;page.on('framenavigated',frame=>{if(frame===page.mainFrame())navigations++;});
  await page.clock.runFor(2200);await page.getByText(`Version ${candidate.version}`,{exact:true}).waitFor();assert.equal(navigations,0);
  // A user loading the candidate during verification also needs the rollback outcome.
  const observer=await instance.newPage();observer.setDefaultTimeout(10000);await observer.clock.install();await observer.goto(url);await observer.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();await observer.getByRole('button',{name:'Settings',exact:true}).click();await observer.getByText(`Version ${candidate.version}`,{exact:true}).waitFor();
  let observerNavigations=0;observer.on('framenavigated',frame=>{if(frame===observer.mainFrame())observerNavigations++;});
  await stop();await completeUpdateRequest(updater,{state:'rolled_back',latest_version:candidate.version,message:'Candidate health checks failed. The previous version was restored.'});await start(originalBinary);
  await page.clock.runFor(2200);await observer.clock.runFor(2200);
  await page.getByText('Candidate health checks failed. The previous version was restored.',{exact:true}).waitFor();
  await observer.getByText(/Candidate health checks failed.*Reload the dashboard to match the running version/).waitFor();
  assert.equal(navigations,0);assert.equal(observerNavigations,0);
  assert.equal(await page.getByRole('dialog',{name:'Dashboard settings',exact:true}).isVisible(),true);
  assert.equal(await observer.getByRole('button',{name:'Reload dashboard',exact:true}).isVisible(),true);
});


test('settings explains an update lock started by another client', {timeout:30000}, async t=>{
  const {url,updater}=await server(t,metrics,10000,{updater:true});const instance=await browser(t);const page=await instance.newPage();page.setDefaultTimeout(10000);
  await page.goto(url);await page.getByText('Synthetic fixture · Demonstration readings, not live hardware.').waitFor();await page.getByRole('button',{name:'Settings',exact:true}).click();
  await page.getByText('Check for a new release when needed.',{exact:true}).waitFor();
  const initial=await (await fetch(`${url}/api/v1/state`)).json();
  await writeUpdateStatus(updater,{state:'available',latest_version:'9.9.9',message:'A new release is available.'});
  const install=await fetch(`${url}/api/v1/software-update/install`,{method:'POST',headers:{'Content-Type':'application/json',Origin:url,'X-OYAsys-CSRF':initial.csrf_token},body:JSON.stringify({version:'9.9.9'})});assert.equal(install.status,202);
  await page.getByLabel('Sampling interval seconds',{exact:true}).fill('20');
  const rejected=page.waitForResponse(response=>response.url().endsWith('/api/v1/settings')&&response.status()===409);
  await page.getByRole('button',{name:'Save settings',exact:true}).click();const reason=(await (await rejected).json()).error;
  await page.getByText(`Settings were not saved: ${reason}`,{exact:true}).waitFor();assert.match(reason,/update|recovery/i);
  assert.equal((await (await fetch(`${url}/api/v1/state`)).json()).settings.sampling_seconds,10);
});
