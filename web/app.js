/* OYAsys dashboard. No runtime framework, remote assets, or animation loop. */
'use strict';

/** @typedef {{id:string,label:string,unit:string,group:string,source:string}} Metric */
/** @typedef {{revision:number,sampling_seconds:number,retention_days:number,p100_fan_id:string|null}} Settings */
/** @typedef {{sequence:number,timestamp_ms:number,values:Record<string,number|null>,persisted:boolean}} Snapshot */
/** @typedef {{inventory:{hostname:string,system:{label:string,value:string}[],metrics:Metric[],warnings:string[],synthetic:boolean,availability?:Record<string,{status:string,reason?:string}>},snapshot:Snapshot|null,settings:Settings,csrf_token:string,storage:{available:boolean,error:string|null,bytes:number,samples:number,oldest_ms:number|null,newest_ms:number|null,estimated_daily_bytes:number|null},now_ms:number}} State */
/** @typedef {[number,number|null]} Point */
/** @typedef {{title:string,metrics:Metric[],element:HTMLElement,plotElement:HTMLElement,current:HTMLElement,legend:HTMLElement,summary:HTMLElement,note:HTMLElement,empty:HTMLElement,plot:any}} Panel */
/** @type {State|null} */
let state = null;
/** @type {Panel[]} */
let panels = [];
/** @type {Map<string,Point[]>} */
let historyData = new Map();
let sequence = 0;
let rangeDuration = 3600000;
/** @type {{from:number,to:number}|null} */
let customRange = null;
let range = {from:0,to:0};
let catalogKey = '';
let reduced = false;
let historyReady = false;
let loading = false;
let refreshAgain = false;
let timer = 0;
let historyGeneration = 0;
let stateController = new AbortController();
let historyController = new AbortController();
let settingsRevision = 0;
let staleSettings = false;
let settingsSaving = false;
let historyError = '';
/** @type {Set<string>|null} */
let selectedMetrics = null;
/** @typedef {{enabled:boolean,current_version:string,state:string,latest_version:string|null,message:string,checked_at:number|null,request_id:string|null}} SoftwareStatus */
/** @type {SoftwareStatus|null} */
let softwareStatus = null;
let softwareTimer = 0;
let softwareDeadline = 0;
let softwareGeneration = 0;
let softwareController = new AbortController();
let softwareRequestPending = false;
let softwareRestarting = false;
let softwareExpired = false;
let softwarePageVersion = '';
let softwareInstallRequest = '';
let softwareObservedInstallRequest = '';
let softwareReloadNeeded = false;
let softwareError = '';
const softwarePollMs = 2000;
const softwareCheckMs = 2 * 60 * 1000;
const softwareInstallMs = 10 * 60 * 1000;
const colors = ['#6fc8e6','#8accad','#b5a2eb','#eeb76b','#74a9ef','#e3a2c3'];
const chartLimit = 2000;
/** @type {Map<string,HTMLElement[]>} */
let readouts = new Map();
/** @typedef {{entries:{name:string,bytes:number}[],total_bytes:number|null,files_bytes:number|null,other_folders_bytes:number,complete:boolean,message:string}} FolderScan */
/** @type {Map<string,{result:FolderScan,at:number}>} */
const folderResults = new Map();
let folderActive = '';
/** @type {AbortController|null} */
let folderController = null;
/** @type {{prefix:string,free:HTMLElement,total:HTMLElement,bar:HTMLElement,segments:HTMLElement[],legend:HTMLElement[],message:HTMLElement,analyze:HTMLButtonElement,analysis:HTMLElement}[]} */
let filesystemViews = [];
/** @param {string} id @returns {HTMLElement} */
function el(id) { const node=document.getElementById(id); if(!node) throw new Error(`Missing element: ${id}`); return node; }
/** @param {string} id @returns {HTMLInputElement} */
function input(id) { return /** @type {HTMLInputElement} */ (el(id)); }
/** @param {string} id @returns {HTMLSelectElement} */
function select(id) { return /** @type {HTMLSelectElement} */ (el(id)); }
/** @param {string} tag @param {string} [className] @param {string} [text] @returns {HTMLElement} */
function node(tag,className='',text='') {const n=document.createElement(tag);n.className=className;n.textContent=text;return n;}
/** @param {string} path @param {RequestInit} [options] */
async function api(path,options={}) {
  const response=await fetch(`/api/v1${path}`,{cache:'no-store',credentials:'same-origin',...options});
  if(!response.ok){let message=`Request failed (${response.status})`;try {const data=await response.json();message=data.error||data.message||message;}catch{}const error=Object.assign(new Error(String(message)),{status:response.status});throw error;}
  return response.json();
}
/** @param {number|null|undefined} value @param {string} unit @returns {string} */
function format(value,unit) {
  if(value==null || !Number.isFinite(value)) return 'Unavailable';
  if(unit==='bytes' || unit==='bytes/s') {const suffix=unit==='bytes/s'?'/s':'';let divisor=1;let label='B';for(const next of ['KiB','MiB','GiB','TiB']){if(Math.abs(value)/divisor<1024)break;divisor*=1024;label=next;}return `${(value/divisor).toLocaleString(undefined,{maximumFractionDigits:divisor===1?0:1})} ${label}${suffix}`;}
  if(unit==='s'){const days=Math.floor(value/86400);const hours=Math.floor(value%86400/3600);const minutes=Math.floor(value%3600/60);return days?`${days}d ${hours}h`:`${hours}h ${minutes}m`;}
  return `${value.toLocaleString(undefined,{maximumFractionDigits:['RPM','MHz'].includes(unit)?0:1})}${unit==='%'?'':' '}${unit}`.trim();
}
/** @param {number|null} value */
function date(value){return value==null?'No retained readings':new Date(value).toLocaleString();}
/** @param {string} id @param {HTMLElement} target */
function bindReading(id,target){const targets=readouts.get(id)||[];targets.push(target);readouts.set(id,targets);}
/** @param {Metric} metric */
function isP100(metric){return metric.group==='gpu' && /p100/i.test(`${metric.source} ${metric.label} ${metric.id}`);}
/** @param {Metric} metric @param {number} index */
function color(metric,index){return metric.unit==='RPM'?'#eeb76b':colors[index%colors.length];}
/** @param {string} title @param {Metric[]} metrics @param {HTMLElement} parent */
function createPanel(title,metrics,parent){
  const article=node('article','panel');const header=node('div','panel-header');const heading=node('h3','panel-title',title);const current=node('span','panel-current');header.append(heading,current);article.append(header);
  const plotElement=node('div','chart');plotElement.setAttribute('aria-hidden','true');article.append(plotElement);
  const empty=node('p','chart-empty','Loading history…');article.append(empty);
  const legend=node('div','chart-legend');metrics.forEach((metric,index)=>{const item=node('span','legend-item');const line=node('span','legend-line');line.style.setProperty('--series-color',color(metric,index));const name=node('span','',metric.label);const value=node('span','legend-value','—');bindReading(metric.id,value);item.append(line,name,value);legend.append(item);});article.append(legend);
  const note=node('p','chart-note');article.append(note);
  const summary=node('details','chart-summary');summary.append(node('summary','','Reading summary'));article.append(summary);parent.append(article);
  const panel={title,metrics,element:article,plotElement,current,legend,summary,note,empty,plot:null};panels.push(panel);
  if(metrics.length===1)bindReading(metrics[0].id,current);
}
/** @param {Metric[]} metrics @param {HTMLElement} parent @param {string} prefix */
function groupedPanels(metrics,parent,prefix){
  const groups=new Map();for(const metric of metrics){const key=metric.unit;const current=groups.get(key)||[];current.push(metric);groups.set(key,current);}
  for(const [unit,list]of groups){for(let i=0;i<list.length;i+=6)createPanel(`${prefix}${groups.size>1?` · ${unit}`:''}${list.length>6?` · ${Math.floor(i/6)+1}`:''}`,list.slice(i,i+6),parent);}
}
function rebuild(){
  if(!state)return;for(const panel of panels){if(panel.plot)panel.plot.destroy();}panels=[];readouts=new Map();
  for(const id of ['p100-charts','gpu-overview','system-panels','sensor-panels','io-panels'])el(id).replaceChildren();
  const metrics=state.inventory.metrics;const gpu=metrics.filter(isP100);const p100temp=gpu.filter(m=>m.id.endsWith('.temperature'));const fan=metrics.find(m=>m.id===state?.settings.p100_fan_id&&m.unit==='RPM');
  if(!selectedMetrics)selectedMetrics=new Set([...p100temp,...(fan?[fan]:[]),...metrics].map(m=>m.id).filter((id,index,ids)=>ids.indexOf(id)===index).slice(0,64));
  const chartMetrics=metrics.filter(m=>selectedMetrics?.has(m.id));
  el('p100-missing').hidden=p100temp.length>0;el('gpu-overview').hidden=gpu.length===0;el('p100-charts').hidden=false;
  el('gpu-identity').textContent=gpu[0]?.source||'Source unavailable';
  const gpuReadings=[...p100temp,...(fan?[fan]:[]),...gpu.filter(m=>!m.id.endsWith('.temperature')&&!m.id.endsWith('.memory_total'))];
  for(const metric of gpuReadings){const item=node('div','gpu-reading');const label=node('span','reading-label',metric===fan?'Verified cooling fan':metric.label.replace(/(?:NVIDIA\s+)?(?:Tesla\s+)?P100\s*/i,''));const value=node('strong','','Unavailable');bindReading(metric.id,value);item.append(label,value);el('gpu-overview').append(item);}
  const chosenTemperature=p100temp.filter(m=>selectedMetrics?.has(m.id));
  if(chosenTemperature.length)createPanel('P100 temperature · °C',chosenTemperature,el('p100-charts'));
  if(fan&&selectedMetrics.has(fan.id))createPanel('P100 cooling fan · RPM',[fan],el('p100-charts'));
  else if(!fan){const empty=node('div','empty-state');const mapped=!!state.settings.p100_fan_id;empty.append(node('h3','',mapped?'Verified cooling fan unavailable':'Cooling fan not verified'),node('p','',mapped?'The verified motherboard channel is no longer discoverable. Check Sources & availability; the saved mapping has been preserved.':'Verify the motherboard channel using the existing Linux cooling profile, then select it in Settings. No fan is assigned automatically.'));el('p100-charts').append(empty);}
  const plotted=new Set(panels.flatMap(p=>p.metrics.map(m=>m.id)));
  let remaining=Math.max(0,64-plotted.size);
  /** @param {string} group */
  const take=group=>{const all=chartMetrics.filter(m=>m.group===group&&!plotted.has(m.id));const result=all.slice(0,remaining);remaining-=result.length;for(const metric of result)plotted.add(metric.id);return result;};
  groupedPanels(take('cpu'),el('system-panels'),'CPU');groupedPanels(take('memory'),el('system-panels'),'Memory');
  groupedPanels(take('thermal'),el('sensor-panels'),'Temperature');groupedPanels(take('fan'),el('sensor-panels'),'Fan speed');
  groupedPanels(take('storage'),el('io-panels'),'Storage');groupedPanels(take('network'),el('io-panels'),'Network');
  groupedPanels(take('system'),el('system-panels'),'System');groupedPanels(take('gpu'),el('system-panels'),'GPU');
  for(const id of ['system-panels','sensor-panels','io-panels']){if(!el(id).children.length)el(id).append(node('p','empty-section','No readings available in this group. See Sources & availability below.'));}
  el('source-count').textContent=`${metrics.length} sensors · ${selectedIds().length} charted`;
  el('p100-charts').hidden=el('p100-charts').children.length===0;
  buildFilesystemCapacity();updateStaticDetails();renderSnapshot();
}
function buildFilesystemCapacity(){
  const parent=el('filesystem-capacity');parent.replaceChildren();filesystemViews=[];
  const filesystems=state?.inventory.metrics.filter(m=>m.group==='storage'&&m.id.startsWith('filesystem.')&&m.id.endsWith('.total'))||[];
  parent.hidden=filesystems.length===0;
  for(const metric of filesystems){
    const article=node('article','filesystem-card');const heading=node('div','filesystem-heading');
    const identity=node('div');identity.append(node('h3','filesystem-name',metric.label.replace(/ capacity$/,'')),node('p','filesystem-source',metric.source));
    const reading=node('div','filesystem-reading');const free=node('strong');const total=node('span');reading.append(free,total);heading.append(identity,reading);
    const bar=node('div','filesystem-bar');bar.setAttribute('aria-hidden','true');
    const segments=['used','reserved','free'].map(kind=>node('span',`filesystem-segment filesystem-${kind}`));bar.append(...segments);
    const details=node('dl','filesystem-legend');const legend=['Used','Reserved','Available'].map((label,index)=>{const group=node('div');const term=node('dt',`filesystem-key filesystem-key-${index}`,label);const value=node('dd');group.append(term,value);details.append(group);return value;});
    const message=node('p','filesystem-message');
    const actions=node('div','filesystem-actions');const analyze=document.createElement('button');analyze.type='button';analyze.textContent='Analyze folders';analyze.addEventListener('click',()=>void analyzeFolders(metric.id));
    actions.append(analyze,node('span','','Scans only when requested. Up to 10 seconds.'));
    const analysis=node('div','folder-analysis');analysis.hidden=true;analysis.setAttribute('role','region');analysis.setAttribute('aria-label',`Folder breakdown for ${metric.label.replace(/ capacity$/,'')}`);
    article.append(heading,bar,details,message,actions,analysis);parent.append(article);
    filesystemViews.push({prefix:metric.id.slice(0,-6),free,total,bar,segments,legend,message,analyze,analysis});
    const saved=folderResults.get(metric.id);if(saved)renderFolderAnalysis(analysis,saved.result,saved.at);
    if(folderActive===metric.id){analysis.hidden=false;analysis.textContent='Analyzing accessible folders…';}
    analyze.disabled=!!folderActive;
  }
}
/** @param {HTMLElement} target @param {FolderScan} result @param {number} at */
function renderFolderAnalysis(target,result,at){
  target.replaceChildren();target.hidden=false;
  const title=node('div','folder-heading');title.append(node('h4','','Folder breakdown'),node('span','',`${result.complete?'Scanned':'Partial scan'} · ${new Date(at).toLocaleTimeString()}`));target.append(title);
  const entries=[...result.entries];
  if(result.other_folders_bytes>0)entries.push({name:'Other folders',bytes:result.other_folders_bytes});
  if(result.files_bytes!=null&&result.files_bytes>0)entries.push({name:'Files & directory metadata at this level',bytes:result.files_bytes});
  const sum=entries.reduce((total,item)=>total+item.bytes,0);const total=result.total_bytes??sum;
  const list=node('ol','folder-list');
  entries.forEach((entry,index)=>{
    const row=node('li','folder-row');const text=node('div','folder-label');const value=node('span','',format(entry.bytes,'bytes'));text.append(node('span','folder-name',entry.name),value);
    const track=node('div','folder-track');track.setAttribute('aria-hidden','true');const fill=node('span');fill.style.width=`${total>0?Math.min(100,entry.bytes/total*100):0}%`;fill.style.background=colors[index%colors.length];track.append(fill);row.append(text,track);list.append(row);
  });
  if(entries.length)target.append(list);else target.append(node('p','muted',result.complete?'No allocated file data found.':'No folder totals were available before the scan ended.'));
  target.append(node('p','folder-note',result.message));
}
/** @param {string} metricId */
async function analyzeFolders(metricId){
  if(!state||folderActive)return;
  folderActive=metricId;folderController=new AbortController();
  for(const view of filesystemViews)view.analyze.disabled=true;
  const target=()=>filesystemViews.find(view=>`${view.prefix}.total`===metricId)?.analysis;
  const initial=target();if(initial){initial.hidden=false;initial.textContent='Analyzing accessible folders…';initial.setAttribute('aria-busy','true');}
  const timeout=setTimeout(()=>folderController?.abort(),15000);
  try{
    const result=await api('/storage/analyze',{method:'POST',headers:{'Content-Type':'application/json','X-OYAsys-CSRF':state.csrf_token},body:JSON.stringify({metric_id:metricId}),signal:folderController.signal});
    const at=Date.now();folderResults.set(metricId,{result,at});const current=target();if(current)renderFolderAnalysis(current,result,at);
  }catch(error){const current=target();if(current){current.replaceChildren(node('p','folder-note',error.name==='AbortError'?'Folder analysis timed out. Try again when disk activity is lower.':error.message));current.hidden=false;}}
  finally{clearTimeout(timeout);folderActive='';folderController=null;const current=target();current?.removeAttribute('aria-busy');renderFilesystemCapacity();}
}
function renderFilesystemCapacity(){
  const values=state?.snapshot?.values;
  for(const view of filesystemViews){
    const total=values?.[`${view.prefix}.total`];const used=values?.[`${view.prefix}.used`];const available=values?.[`${view.prefix}.available`];
    const valid=typeof total==='number'&&Number.isFinite(total)&&total>0&&typeof used==='number'&&Number.isFinite(used)&&used>=0&&used<=total&&typeof available==='number'&&Number.isFinite(available)&&available>=0&&available<=total-used;
    view.bar.hidden=!valid;view.analyze.disabled=!!folderActive||!valid;
    if(!valid){view.free.textContent='Unavailable';view.total.textContent='Capacity reading unavailable';for(const item of view.legend)item.textContent='—';view.message.textContent='Waiting for a complete filesystem reading. See Sources & availability for details.';continue;}
    const reserved=Math.max(0,total-used-available);const amounts=[used,reserved,available];
    view.free.textContent=`${format(available,'bytes')} available`;view.total.textContent=`of ${format(total,'bytes')} capacity`;
    amounts.forEach((amount,index)=>{view.segments[index].style.width=`${amount/total*100}%`;view.legend[index].textContent=`${format(amount,'bytes')} · ${format(amount/total*100,'%')}`;});
    view.message.textContent=reserved>0?'Reserved space is free space the filesystem does not report as available.':'No reserved space reported.';
  }
}
/** @param {HTMLElement} target @param {[string,string][]} values */
function setDetails(target,values){target.replaceChildren();for(const[label,value]of values)target.append(node('dt','',label),node('dd','',value));}
function updateStaticDetails(){
  if(!state)return;setDetails(el('inventory'),state.inventory.system.length?state.inventory.system.map(s=>[s.label,s.value]):[['Inventory','No specifications reported']]);
  const warningEl=el('source-warnings');warningEl.replaceChildren();for(const warning of state.inventory.warnings)warningEl.append(node('p','',warning));
  const body=el('source-table');body.replaceChildren();for(const metric of state.inventory.metrics){const row=node('tr');const value=node('td');value.dataset.showReason='true';bindReading(metric.id,value);const choice=node('td');const checkbox=document.createElement('input');checkbox.type='checkbox';checkbox.checked=selectedMetrics?.has(metric.id)||false;checkbox.dataset.metricId=metric.id;checkbox.setAttribute('aria-label',`Chart ${metric.label} (${metric.id})`);checkbox.addEventListener('change',()=>{if(document.querySelectorAll('#source-table input:checked').length>64){checkbox.checked=false;el('source-selection-message').textContent='64 chart sources maximum. Uncheck another source first.';}else el('source-selection-message').textContent='Selection changed. Apply to update the charts.';});choice.append(checkbox);row.append(choice,node('td','',metric.label),value,node('td','',`${metric.source} · ${metric.id}`));body.append(row);}
}
function renderSnapshot(){
  if(!state||document.hidden)return;
  renderFilesystemCapacity();
  el('synthetic-notice').hidden=!state.inventory.synthetic;
  const snapshot=state.snapshot;
  for(const[id,targets]of readouts){
    const metric=state.inventory.metrics.find(m=>m.id===id);if(!metric)continue;
    const value=snapshot?.values[id];const availability=state.inventory.availability?.[id];
    const labels={available:'Unavailable',warming_up:'Warming up',unsupported:'Unsupported',permission_denied:'Permission denied',lost:'Source lost',unavailable:'Unavailable',invalid:'Invalid reading'};
    for(const target of targets){
      const reading=value==null&&availability?(labels[availability.status]||'Unavailable'):format(value,metric.unit);
      target.textContent=reading+(value==null&&availability?.reason&&target.dataset.showReason==='true'?` · ${availability.reason}`:'');
      target.title=value==null?availability?.reason||'':'';
    }
  }
  const age=snapshot?state.now_ms-snapshot.timestamp_ms:0;
  const status=!snapshot?'Collecting':age>Math.max(30000,state.settings.sampling_seconds*3000)?'Stale readings':state.inventory.synthetic?'Synthetic':'Collecting';
  el('connection-status').textContent=status;el('connection-status').dataset.state=status==='Collecting'&&snapshot?'current':'';
  el('collection-detail').textContent=snapshot?`Latest ${new Date(snapshot.timestamp_ms).toLocaleTimeString()} · Every ${state.settings.sampling_seconds}s · ${state.settings.retention_days} days retained`:'Waiting for the first collection.';
  const messages=[];if(!state.storage.available)messages.push(`History unavailable: ${state.storage.error||'storage could not be opened'}. Live readings may still be available.`);else if(snapshot&&!snapshot.persisted)messages.push('Latest collection was not saved. Live readings may contain a gap in history.');if(status==='Stale readings')messages.push('The latest reading is stale. Values below show the last collection, not a current measurement.');if(historyError)messages.push(historyError);
  el('data-notice').hidden=messages.length===0;el('data-notice').textContent=messages.join(' ');
  setDetails(el('storage-details'),[['Sampling interval',`${state.settings.sampling_seconds} seconds`],['Full-detail retention',`${state.settings.retention_days} days`],['Stored collections',state.storage.samples.toLocaleString()],['History storage',format(state.storage.bytes,'bytes')],['Oldest reading',date(state.storage.oldest_ms)],['Persistence',state.storage.available?(snapshot?.persisted===false?'Latest collection not saved':'Available'):'Unavailable']]);
}
/** Render only lone readings: continuous runs already have a line, gaps never get bridged.
 * @param {(number|null|undefined)[]} values @returns {number[]|null}
 */
function isolatedPoints(values){
  const indices=[];let candidate=-1;let previousFinite=false;
  for(let index=0;index<values.length;index++){
    const value=values[index];if(value===undefined)continue;
    if(value===null){if(candidate>=0)indices.push(candidate);candidate=-1;previousFinite=false;}
    else{candidate=previousFinite?-1:index;previousFinite=true;}
  }
  if(candidate>=0)indices.push(candidate);return indices.length?indices:null;
}
/** @param {Panel} panel */
function renderPanel(panel){
  if(!state||document.hidden)return;
  const sets=panel.metrics.map(m=>historyData.get(m.id)||[]);
  const timestamps=[...new Set(sets.flatMap(points=>points.map(p=>p[0])))].sort((a,b)=>a-b);
  const hasValues=sets.some(points=>points.some(p=>p[1]!=null));
  panel.empty.hidden=hasValues;panel.plotElement.hidden=!hasValues;
  if(!hasValues){panel.empty.textContent=historyError?'History could not be loaded. Retry with Refresh.':!historyReady?'Loading history…':'No readings in this range. Try a wider range or wait for collection.';if(panel.plot){panel.plot.destroy();panel.plot=null;}}
  else{
    // Undefined entries align independent reduced series; explicit nulls preserve real gaps.
    const data=[timestamps.map(t=>t/1000),...sets.map(points=>{const map=new Map(points);return timestamps.map(t=>map.has(t)?map.get(t):undefined);})];
    const width=Math.max(180,Math.floor(panel.plotElement.getBoundingClientRect().width));
    if(!panel.plot){
      const UPlot=/** @type {any} */(window).uPlot;
      panel.plot=new UPlot({width,height:175,padding:[12,12,0,0],legend:{show:false},cursor:{sync:{key:'oyasys-time',scales:['x',null]},drag:{x:true,y:false,setScale:false}},scales:{x:{time:true,min:range.from/1000,max:range.to/1000},y:{auto:true}},axes:[{stroke:'#a8b3c2',grid:{stroke:'#29313d',width:1},ticks:{show:false},font:'10px system-ui',size:31,space:85},{stroke:'#a8b3c2',grid:{stroke:'#29313d',width:1},ticks:{show:false},font:'10px system-ui',size:62,values:(_u,values)=>values.map(v=>format(v,panel.metrics[0].unit))}],series:[{},...panel.metrics.map((metric,index)=>({label:metric.label,stroke:color(metric,index),width:1.5,spanGaps:false,points:{show:false,size:5,filter:(plot,seriesIndex)=>isolatedPoints(plot.data[seriesIndex])},value:(_u,value)=>format(value,metric.unit)}))],hooks:{setSelect:[u=>{if(u.select.width<5)return;const from=Math.floor(u.posToVal(u.select.left,'x')*1000);const to=Math.ceil(u.posToVal(u.select.left+u.select.width,'x')*1000);if(to>from){setCustomRange(from,to);void reloadHistory();}}]}},data,panel.plotElement);
    }else{panel.plot.setData(data,false);panel.plot.setScale('x',{min:range.from/1000,max:range.to/1000});if(panel.plot.width!==width)panel.plot.setSize({width,height:175});}
  }
  panel.note.textContent=reduced?'Display reduced · Sampled peaks and gaps preserved. Drag to zoom.':'Full-detail readings · Drag to zoom.';
  const table=node('table','summary-table');const head=node('thead');const headRow=node('tr');for(const label of ['Sensor','Minimum','Maximum','Latest in range'])headRow.append(node('th','',label));head.append(headRow);const body=node('tbody');
  panel.metrics.forEach((metric,index)=>{const values=sets[index].filter(p=>p[1]!=null).map(p=>/** @type {number} */(p[1]));const row=node('tr');row.append(node('td','',metric.label),node('td','',format(values.length?Math.min(...values):null,metric.unit)),node('td','',format(values.length?Math.max(...values):null,metric.unit)),node('td','',format(sets[index].at(-1)?.[1],metric.unit)));body.append(row);});table.append(head,body);panel.summary.querySelector('table')?.remove();panel.summary.append(table);
}
function renderCharts(){if(document.hidden)return;for(const panel of panels)renderPanel(panel);}
function selectedIds(){return [...new Set(panels.flatMap(p=>p.metrics.map(m=>m.id)))].slice(0,64);}
function activeRange(){const now=state?.now_ms||Date.now();return customRange||{from:Math.max(0,now-rangeDuration),to:now};}
async function reloadHistory(){
  if(!state||document.hidden)return;historyController.abort();historyController=new AbortController();const generation=++historyGeneration;range=activeRange();historyReady=false;historyError='';historyData=new Map();renderCharts();
  const ids=selectedIds();if(!ids.length){historyReady=true;return;}
  try{
    const width=Math.max(...panels.map(p=>p.plotElement.clientWidth),300);const points=Math.min(1600,Math.max(200,Math.floor(width*1.5)));
    const query=new URLSearchParams({from:String(range.from),to:String(range.to),metrics:ids.join(','),points:String(points)});
    const result=await api(`/history?${query}`,{signal:historyController.signal});if(generation!==historyGeneration||document.hidden)return;
    historyData=new Map(result.series.map(series=>[series.id,series.points]));reduced=result.reduced;historyReady=true;historyError='';renderCharts();renderSnapshot();
  }catch(error){if(error.name==='AbortError')return;historyError=`History unavailable: ${error.message}`;historyReady=true;renderCharts();renderSnapshot();}
}
/** @param {Snapshot[]} snapshots */
function appendSnapshots(snapshots){
  if(!historyReady||customRange||!snapshots.length||!state)return false;
  range=activeRange();let overflow=false;
  for(const id of selectedIds()){
    const points=(historyData.get(id)||[]).filter(p=>p[0]>=range.from&&p[0]<=range.to);
    for(const snapshot of snapshots){if(snapshot.timestamp_ms<range.from||snapshot.timestamp_ms>range.to||snapshot.timestamp_ms<=(points.at(-1)?.[0]||0))continue;
      const previous=points.at(-1);if(previous&&snapshot.timestamp_ms-previous[0]>state.settings.sampling_seconds*1500)points.push([previous[0]+1,null]);
      points.push([snapshot.timestamp_ms,snapshot.persisted?(snapshot.values[id]??null):null]);}
    if(points.length>chartLimit)overflow=true;historyData.set(id,points.slice(-chartLimit));
  }
  if(!overflow)renderCharts();return overflow;
}
async function refresh(){
  if(document.hidden)return;if(loading){refreshAgain=true;return;}loading=true;clearTimeout(timer);const button=/** @type {HTMLButtonElement} */(el('refresh'));button.disabled=true;
  stateController=new AbortController();
  try{
    const previous=state;
    // One batched request per cadence: updates carries the current state plus missed samples.
    let updates=null;let updatesFailure='';
    if(previous){try{updates=await api(`/updates?after=${sequence}&limit=1000`,{signal:stateController.signal});}
      catch(error){if(error.name==='AbortError')throw error;updatesFailure=`History updates unavailable: ${error.message}. Live readings are still available.`;}}
    // Only an updates error adds a state request; storage/query failure must not hide live telemetry.
    const fresh=/** @type {State} */(updates?updates.state:await api('/state',{signal:stateController.signal}));if(document.hidden)return;
    if(updatesFailure)historyError=updatesFailure;
    else if(historyError.startsWith('History updates unavailable:'))historyError='';
    state=fresh;
    const key=JSON.stringify([fresh.inventory.metrics,fresh.settings.p100_fan_id]);const changed=key!==catalogKey;
    if(changed){catalogKey=key;rebuild();}
    const settingsChanged=!!previous&&previous.settings.revision!==fresh.settings.revision;
    if(settingsChanged&&!el('settings-drawer').hidden){staleSettings=true;el('settings-message').textContent='Settings changed in another session. Reopen Settings to load the current values.';}
    renderSnapshot();
    if(updatesFailure){if(changed||settingsChanged){historyReady=false;historyData=new Map();renderCharts();}return;}
    const incompleteUpdates=updates&&fresh.snapshot?.persisted&&(updates.next_after??sequence)<fresh.snapshot.sequence;
    if(!previous||changed||settingsChanged||!historyReady||updates?.reset_required||incompleteUpdates){sequence=fresh.snapshot?.sequence||0;await reloadHistory();}
    else{
      sequence=updates.next_after??sequence;
      const newest=updates.snapshots.at(-1);if(newest&&(!state.snapshot||newest.sequence>state.snapshot.sequence)){state.snapshot=newest;state.now_ms=Math.max(state.now_ms,newest.timestamp_ms);renderSnapshot();}
      if(appendSnapshots(updates.snapshots)||historyError)await reloadHistory();
    }
  }catch(error){if(error.name!=='AbortError'){el('connection-status').textContent=softwareStatus?.state==='installing'?'Restarting':'Disconnected';el('connection-status').dataset.state='';el('data-notice').hidden=false;el('data-notice').textContent=softwareStatus?.state==='installing'?'OYAsys is restarting for the update. Collection resumes when the service returns.':`Cannot reach the server. ${state?'Showing the last received readings. ':''}Retry with Refresh; reconnection is automatic.`;}}
  finally{loading=false;button.disabled=false;if(!document.hidden){if(refreshAgain){refreshAgain=false;void refresh();}else schedule();}}
}
function schedule(){clearTimeout(timer);timer=window.setTimeout(()=>void refresh(),Math.max(1000,(state?.settings.sampling_seconds||10)*1000));}
/** @param {number} time */
function localInputTime(time){const d=new Date(time);return new Date(time-d.getTimezoneOffset()*60000).toISOString().slice(0,16);}
/** @param {number} from @param {number} to */
function setCustomRange(from,to){customRange={from,to};select('range').value='custom';el('custom-range').hidden=false;input('range-from').value=localInputTime(from);input('range-to').value=localInputTime(to);}
select('range').addEventListener('change',()=>{if(select('range').value==='custom'){el('custom-range').hidden=false;const current=activeRange();input('range-from').value=localInputTime(current.from);input('range-to').value=localInputTime(current.to);input('range-from').focus();return;}customRange=null;rangeDuration=Number(select('range').value);el('custom-range').hidden=true;el('range-error').textContent='';void reloadHistory();});
el('custom-range').addEventListener('submit',event=>{event.preventDefault();const from=new Date(input('range-from').value).getTime();const to=new Date(input('range-to').value).getTime();const earliest=(state?.now_ms||Date.now())-(state?.settings.retention_days||30)*86400000;if(!Number.isFinite(from)||!Number.isFinite(to)||to<=from){el('range-error').textContent='Choose an end time after the start time.';return;}if(from<earliest||to>(state?.now_ms||Date.now())+60000){el('range-error').textContent=`Choose a range within the last ${state?.settings.retention_days||30} days.`;return;}el('range-error').textContent='';setCustomRange(from,to);void reloadHistory();});

el('apply-sources').addEventListener('click',()=>{const choices=Array.from(document.querySelectorAll('#source-table input:checked'));selectedMetrics=new Set(choices.map(choice=>/** @type {HTMLInputElement} */(choice).dataset.metricId||''));rebuild();el('source-selection-message').textContent='Chart sources applied.';void reloadHistory();});
el('reset-sources').addEventListener('click',()=>{selectedMetrics=null;rebuild();el('source-selection-message').textContent='Default chart sources restored.';void reloadHistory();});
el('refresh').addEventListener('click',async()=>{
  if(!state){void refresh();return;}if(loading)return;clearTimeout(timer);const button=/** @type {HTMLButtonElement} */(el('refresh'));button.disabled=true;
  try{await api('/refresh',{method:'POST',headers:{'X-OYAsys-CSRF':state.csrf_token}});catalogKey='';await refresh();}
  catch(error){el('data-notice').hidden=false;el('data-notice').textContent=`Sources could not be refreshed: ${error.message}. Try again.`;schedule();}
  finally{button.disabled=false;}
});
function closeSettings(){stopSoftwarePolling();/** @type {HTMLDialogElement} */(el('settings-drawer')).close();el('settings-drawer').hidden=true;el('open-settings').setAttribute('aria-expanded','false');el('open-settings').focus();}
function openSettings(){
  if(!state)return;settingsRevision=state.settings.revision;staleSettings=false;input('sampling').value=String(state.settings.sampling_seconds);input('retention').value=String(state.settings.retention_days);input('confirm-delete').checked=false;
  const mapping=select('fan-mapping');mapping.replaceChildren(new Option('Not verified',''));for(const metric of state.inventory.metrics.filter(m=>m.unit==='RPM'))mapping.add(new Option(`${metric.label} · ${metric.source}`,metric.id));if(state.settings.p100_fan_id&&!state.inventory.metrics.some(m=>m.id===state.settings.p100_fan_id))mapping.add(new Option(`Saved source unavailable · ${state.settings.p100_fan_id}`,state.settings.p100_fan_id));mapping.value=state.settings.p100_fan_id||'';
  el('settings-message').textContent='';el('settings-message').dataset.error='false';el('settings-drawer').hidden=false;/** @type {HTMLDialogElement} */(el('settings-drawer')).showModal();el('open-settings').setAttribute('aria-expanded','true');settingsImpact();if(softwareStatus?.state==='installing')el('close-settings').focus();else input('sampling').focus();softwareDeadline=0;softwareExpired=false;void loadSoftwareStatus();
}
function settingsImpact(){
  if(!state)return;renderSoftwareStatus();const sampling=Number(input('sampling').value);const retention=Number(input('retention').value);const shorter=retention<state.settings.retention_days;el('delete-confirmation').hidden=!shorter;input('confirm-delete').required=shorter;
  const daily=state.storage.estimated_daily_bytes;
  el('storage-estimate').textContent=daily&&sampling>0&&retention>0?`Estimated history: ${format(daily*state.settings.sampling_seconds/sampling*retention,'bytes')} at these settings, based on observed storage. Actual use varies.`:'Storage estimate appears after enough readings are collected. Sampling every second stores 10× as many collections as every 10 seconds.';
}
el('open-settings').addEventListener('click',openSettings);el('close-settings').addEventListener('click',closeSettings);el('cancel-settings').addEventListener('click',closeSettings);input('sampling').addEventListener('input',settingsImpact);input('retention').addEventListener('input',settingsImpact);select('fan-mapping').addEventListener('change',renderSoftwareStatus);el('settings-drawer').addEventListener('cancel',event=>{event.preventDefault();closeSettings();});
el('settings-drawer').addEventListener('keydown',event=>{
  if(event.key!=='Tab')return;
  const controls=Array.from(el('settings-drawer').querySelectorAll('button:not(:disabled),input:not(:disabled),select:not(:disabled)')).filter(control=>/** @type {HTMLElement} */(control).offsetParent!==null);
  const first=/** @type {HTMLElement} */(controls[0]);const last=/** @type {HTMLElement} */(controls.at(-1));
  if(event.shiftKey&&document.activeElement===first){event.preventDefault();last.focus();}
  else if(!event.shiftKey&&document.activeElement===last){event.preventDefault();first.focus();}
});
el('settings-form').addEventListener('submit',async event=>{
  event.preventDefault();if(!state)return;const message=el('settings-message');message.dataset.error='false';message.textContent='';
  if(staleSettings){message.dataset.error='true';message.textContent='Settings changed elsewhere. Close and reopen Settings before saving.';return;}
  const sampling=Number(input('sampling').value),retention=Number(input('retention').value);
  if(!Number.isInteger(sampling)||sampling<1||sampling>3600||!Number.isInteger(retention)||retention<1||retention>365){message.dataset.error='true';message.textContent='Use whole seconds from 1–3,600 and whole days from 1–365.';return;}
  const button=/** @type {HTMLButtonElement} */(el('save-settings'));settingsSaving=true;button.disabled=true;message.textContent='Saving…';
  try{
    const saved=await api('/settings',{method:'PUT',headers:{'Content-Type':'application/json','X-OYAsys-CSRF':state.csrf_token},body:JSON.stringify({revision:settingsRevision,sampling_seconds:sampling,retention_days:retention,p100_fan_id:select('fan-mapping').value||null,confirm_delete:input('confirm-delete').checked})});
    state.settings=saved;settingsRevision=saved.revision;message.textContent='Settings saved. Collection uses the new settings.';input('confirm-delete').checked=false;settingsImpact();catalogKey='';await refresh();
  }catch(error){message.dataset.error='true';message.textContent=`Settings were not saved: ${error.message}`;}
  finally{settingsSaving=false;button.disabled=false;renderSoftwareStatus();}
});
document.addEventListener('visibilitychange',()=>{clearTimeout(timer);if(document.hidden){stateController.abort();historyController.abort();historyGeneration++;stopSoftwarePolling();}else{void refresh();if(!el('settings-drawer').hidden&&softwareActive()&&!softwareExpired)void loadSoftwareStatus();}});
let resizeTimer=0;window.addEventListener('resize',()=>{clearTimeout(resizeTimer);resizeTimer=window.setTimeout(()=>{if(!document.hidden)renderCharts();},150);});
function softwareVisible(){return !document.hidden&&!el('settings-drawer').hidden;}
function softwareActive(){return softwareStatus?.state==='checking'||softwareStatus?.state==='installing';}
function settingsDirty(){return !!state&&(Number(input('sampling').value)!==state.settings.sampling_seconds||Number(input('retention').value)!==state.settings.retention_days||(select('fan-mapping').value||null)!==state.settings.p100_fan_id);}
function stopSoftwarePolling(){clearTimeout(softwareTimer);softwareController.abort();softwareGeneration++;}
function renderSoftwareStatus(){
  const status=softwareStatus;const busy=softwareActive()||softwareRequestPending;const installing=status?.state==='installing';
  const check=/** @type {HTMLButtonElement} */(el('check-software'));const install=/** @type {HTMLButtonElement} */(el('install-software'));
  check.disabled=!status?.enabled||busy||softwareReloadNeeded;check.textContent=status?.state==='checking'?'Checking…':'Check for updates';
  install.hidden=!status?.enabled||!['available','installing'].includes(status.state)||softwareReloadNeeded;
  install.disabled=busy||settingsDirty();install.textContent=installing?'Updating…':`Update to ${status?.latest_version||'latest version'}`;
  el('software-version').textContent=status?`Version ${status.current_version}`:'Version —';
  el('software-latest').hidden=!status?.latest_version||!['available','installing'].includes(status.state);
  el('software-latest').textContent=status?.latest_version?`Available release · ${status.latest_version}`:'';
  el('software-impact').hidden=!status?.enabled||!['available','installing'].includes(status.state);
  el('software-unsaved').hidden=!settingsDirty()||status?.state!=='available';
  el('reload-software').hidden=!softwareReloadNeeded;
  const message=el('software-status');message.dataset.error=String(!!softwareError||['error','rolled_back'].includes(status?.state||''));
  message.textContent=softwareReloadNeeded?`${status?.message||'The server software changed.'} Reload the dashboard to match the running version. Unsaved settings will be discarded.`:softwareExpired?'The operation is taking longer than expected. Status polling stopped. Reopen Settings to check progress.':softwareRestarting?'Restarting OYAsys… Reconnecting to the updated service.':softwareError||(!status?'Loading update status…':!status.enabled?'Dashboard updates need one-time setup on the server. Enable them using the installation guide.':status.message||({idle:'Check when you want to see whether a new release is available.',checking:'Checking for a new release…',available:'A new release is ready to install.',current:'You are running the latest release.',installing:'Installing the update. The service will restart shortly.',updated:'Update completed.',rolled_back:'Update could not be completed. The previous version was restored.',error:'The update operation failed. Check again to retry.'}[status.state]||'Update status unavailable.'));
  el('software-checked').textContent=status?.checked_at?`Last checked ${new Date(status.checked_at*1000).toLocaleString()}`:'';
  const form=el('settings-form');if(installing&&form.contains(document.activeElement))el('close-settings').focus();
  for(const control of Array.from(form.querySelectorAll('input,select,button')))/** @type {HTMLInputElement} */(control).disabled=!!installing||settingsSaving;
}
function scheduleSoftwareStatus(){
  clearTimeout(softwareTimer);if(!softwareVisible()||!softwareActive()||softwareRequestPending)return;
  if(!softwareDeadline)softwareDeadline=Date.now()+(softwareStatus?.state==='installing'?softwareInstallMs:softwareCheckMs);
  if(Date.now()>=softwareDeadline){softwareExpired=true;renderSoftwareStatus();return;}
  softwareTimer=window.setTimeout(()=>void loadSoftwareStatus(),softwarePollMs);
}
/** @param {SoftwareStatus} status */
function acceptSoftwareStatus(status){
  if(!softwarePageVersion)softwarePageVersion=status.current_version;
  if(status.state==='installing'&&status.request_id)softwareObservedInstallRequest=status.request_id;
  const versionChanged=status.current_version!==softwarePageVersion;
  const observedInstall=!!status.request_id&&(status.request_id===softwareInstallRequest||status.request_id===softwareObservedInstallRequest);
  // A candidate can answer HTTP before the privileged helper finishes health checks.
  // Keep the drawer and polling alive until that helper confirms the terminal outcome.
  const verifiedUpdate=status.state==='updated'&&(versionChanged||observedInstall);
  softwareStatus=status;softwareError='';softwareRestarting=false;
  softwareReloadNeeded=!softwareActive()&&(versionChanged||verifiedUpdate);
  if(softwareReloadNeeded){
    stopSoftwarePolling();
    if(verifiedUpdate&&!settingsDirty()){location.reload();return;}
  }
  if(!softwareActive()){softwareDeadline=0;softwareExpired=false;}
  renderSoftwareStatus();scheduleSoftwareStatus();
}
async function loadSoftwareStatus(){
  if(!softwareVisible()||softwareRequestPending)return;
  softwareController.abort();softwareController=new AbortController();const controller=softwareController;const generation=++softwareGeneration;let timedOut=false;const timeout=window.setTimeout(()=>{timedOut=true;controller.abort();},8000);
  try{const status=await api('/software-update',{signal:softwareController.signal});if(generation!==softwareGeneration||!softwareVisible())return;acceptSoftwareStatus(status);}
  catch(error){if(error.name==='AbortError'&&!timedOut)return;if(generation!==softwareGeneration||!softwareVisible())return;
    softwareRestarting=softwareStatus?.state==='installing';softwareError=softwareRestarting?'':`Cannot read update status: ${timedOut?'request timed out':error.message}. Reopen Settings to retry.`;renderSoftwareStatus();scheduleSoftwareStatus();}
  finally{clearTimeout(timeout);}
}
/** @param {'check'|'install'} action */
async function requestSoftwareAction(action){
  if(!state||!softwareStatus?.enabled||softwareActive()||softwareRequestPending)return;
  if(action==='install'&&(softwareStatus.state!=='available'||!softwareStatus.latest_version||settingsDirty()))return;
  const previousStatus=softwareStatus;softwareStatus={...softwareStatus,state:action==='install'?'installing':'checking',message:action==='install'?'Starting the update…':'Checking for a new release…'};
  softwareRequestPending=true;softwareError='';softwareExpired=false;softwareDeadline=Date.now()+(softwareStatus?.state==='installing'?softwareInstallMs:softwareCheckMs);clearTimeout(softwareTimer);renderSoftwareStatus();
  try{
    const options={method:'POST',signal:AbortSignal.timeout(15000),headers:{'Content-Type':'application/json','X-OYAsys-CSRF':state.csrf_token},...(action==='install'?{body:JSON.stringify({version:softwareStatus.latest_version})}:{})};
    const status=await api(`/software-update/${action}`,options);
    if(action==='install')softwareInstallRequest=status.request_id||'';
    softwareRequestPending=false;acceptSoftwareStatus(status);
  }catch(error){softwareRequestPending=false;if(error.status)softwareStatus=previousStatus;softwareError=`${action==='install'?'Update':'Check'} could not be confirmed: ${error.message}. Reopen Settings to refresh the status before retrying.`;renderSoftwareStatus();if(!error.status)void loadSoftwareStatus();}
}
el('check-software').addEventListener('click',()=>void requestSoftwareAction('check'));
el('install-software').addEventListener('click',()=>void requestSoftwareAction('install'));
el('reload-software').addEventListener('click',()=>location.reload());
void refresh();
