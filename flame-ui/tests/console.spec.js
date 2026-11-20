import { test, expect } from '@playwright/test';
const app = (name,state=0) => ({metadata:{name},spec:{shim:3,command:'/usr/local/flame/bin/flmping-service',description:'Application service'},status:{state,creation_time:1794614400000}});
const inventory = {
  applications:[app('flmping'),app('flmrun',1),...Array.from({length:13},(_,i)=>app(`python-${i}`))],
  nodes:[{metadata:{name:'flm-worker-01'},spec:{hostname:'flm-worker-01',shims:[0,3]},status:{state:1,capacity:{cpu:8,memory:16*1024**3,gpu:1},allocatable:{cpu:6},last_heartbeat_time:1794614400,info:{arch:'x86_64',os:'linux',os_image:'Debian GNU/Linux',kernel_version:'6.12'},addresses:[{type:'InternalIP',address:'192.168.3.8'}]}}],
  sessions:[{metadata:{id:'session-42'},spec:{application:'flmping',priority:1},status:{state:0,running:2,pending:3,succeed:16,failed:0,creation_time:1794614400000}}],
  executors:[{metadata:{id:'executor-01'},spec:{node:'flm-worker-01',application:'flmping',shim:3,resreq:{cpu:2,memory:1024**3}},status:{state:4,session_id:'session-42'}}],
};
const stateNames = {nodes:['Unknown','Ready','Not ready'],applications:['Enabled','Disabled'],sessions:['Open','Closed','Suspending','Suspended'],executors:['Unknown','Void','Idle','Binding','Bound','Unbinding','Releasing','Released']};
async function fixture(page, options={}) {
  const data=structuredClone(inventory);
  await page.route('**/api/**',async route=>{
    const url=new URL(route.request().url()),resource=url.pathname.split('/').at(-1);
    if(url.pathname==='/api/connection') return route.fulfill({json:{endpoint:'http://flm:8080',displayName:'Training Flame'}});
    if(route.request().method()==='POST') {
      if(options.actionFail) return route.fulfill({status:409,json:{error:'Session state changed. Refresh and retry.'}});
      if(resource==='delete') data.sessions=[];
      else data.sessions[0].status.state={close:1,suspend:2,resume:0}[resource];
      return route.fulfill({json:{session:data.sessions[0]||null}});
    }
    const currentPage=Number(url.searchParams.get('page')||1),pageSize=Number(url.searchParams.get('pageSize')||12);
    if(resource==='tasks') return route.fulfill({json:{items:[{metadata:{id:`task-${currentPage}`},status:{state:2,creation_time:1794614400000}}],page:currentPage,pageSize,total:null,hasNext:currentPage===1}});
    if(options.fail) return route.fulfill({status:502,json:{error:'FLM is unavailable'}});
    if(!Object.hasOwn(data,resource)) return route.fulfill({status:404,json:{error:'Unknown resource'}});
    if(options.delays?.[resource]) await new Promise(resolve=>setTimeout(resolve,options.delays[resource]));
    const filter=url.searchParams.get('state'),rows=data[resource].filter(row=>!filter||stateNames[resource][row.status.state]===filter);
    return route.fulfill({json:{items:rows.slice((currentPage-1)*pageSize,currentPage*pageSize),total:rows.length,page:currentPage,pageSize,hasNext:currentPage*pageSize<rows.length}});
  });
}
test('root is the console and reads only its active Flame resource',async({page})=>{
  const paths=[];page.on('request',r=>{if(new URL(r.url()).pathname.startsWith('/api/'))paths.push(new URL(r.url()).pathname)});
  await fixture(page);await page.goto('/');
  await expect(page).toHaveURL(/\/#\/applications$/);
  await expect(page.getByRole('heading',{name:'Applications',exact:true,level:1})).toBeVisible();
  await expect(page.getByRole('button',{name:/flmping/})).toBeVisible();
  await expect(page.locator('#connection-name')).toHaveText('Training Flame');
  expect(paths.every(p=>['/api/connection','/api/applications'].includes(p))).toBe(true);
  await expect(page.getByRole('link',{name:'Settings',exact:true})).toHaveCount(0);
  expect((await page.request.get('/dashboard')).status()).toBe(404);
  expect((await page.request.get('/console')).status()).toBe(404);
});
test('resource pagination and filters use upstream pages and reset page one',async({page})=>{
  await fixture(page);await page.goto('/');
  await page.getByRole('button',{name:'Next →',exact:true}).click();
  await expect(page.locator('#page-number')).toHaveText('Page 2 of 2');
  await expect(page.getByRole('button',{name:/python-12/})).toBeVisible();
  const request=page.waitForRequest(r=>new URL(r.url()).pathname==='/api/applications'&&new URL(r.url()).searchParams.get('state')==='Disabled');
  await page.getByLabel('Filter by status').selectOption('Disabled');
  expect(new URL((await request).url()).searchParams.get('page')).toBe('1');
  await expect(page.getByRole('button',{name:/flmrun/})).toBeVisible();
  await expect(page.getByRole('button',{name:/flmping/})).toHaveCount(0);
});
test('nodes show only direct Flame status and details',async({page})=>{
  await fixture(page);await page.goto('/#/nodes');
  await expect(page.getByRole('columnheader',{name:'Configured',exact:true})).toHaveCount(0);
  await expect(page.locator('#content tbody').getByText('Ready',{exact:true})).toBeVisible();
  await page.getByRole('button',{name:/flm-worker-01/}).click();
  await expect(page.getByRole('dialog')).toContainText('Debian GNU/Linux');
  await expect(page.getByRole('dialog')).toContainText('192.168.3.8');
  await expect(page.getByRole('dialog')).not.toContainText('Agent');
});
test('session task drawer retains task paging and progress',async({page})=>{
  await fixture(page);await page.goto('/#/sessions');
  await expect(page.getByRole('progressbar').first()).toHaveAttribute('aria-valuenow','76');
  await page.getByRole('button',{name:'session-42',exact:true}).click();
  await expect(page.getByRole('dialog')).toContainText('task-1');
  await page.getByRole('button',{name:'Next tasks →'}).click();
  await expect(page.getByRole('dialog')).toContainText('task-2');
  await expect(page.getByRole('button',{name:'Next tasks →'})).toBeDisabled();
});
test('session commands use direct FLM actions and update available commands',async({page})=>{
  await fixture(page);await page.goto('/#/sessions');page.on('dialog',dialog=>dialog.accept());
  const mutation=page.waitForRequest(r=>r.method()==='POST'&&new URL(r.url()).pathname==='/api/sessions/session-42/actions/close');
  await page.getByRole('button',{name:'Close',exact:true}).click();await mutation;
  await expect(page.locator('#content').getByText('Closed',{exact:true})).toBeVisible();
  await expect(page.getByRole('button',{name:'Delete',exact:true})).toBeVisible();
  await page.getByRole('button',{name:'Delete',exact:true}).click();
  await expect(page.getByRole('heading',{name:'No sessions yet'})).toBeVisible();
});
test('mutation errors remain visible and preserve session state',async({page})=>{
  await fixture(page,{actionFail:true});await page.goto('/#/sessions');
  await page.getByRole('button',{name:'Suspend',exact:true}).click();
  await expect(page.locator('#notice')).toContainText('Session state changed');
  await expect(page.locator('#content').getByText('Open',{exact:true})).toBeVisible();
});
test('upstream failure is visible without fake healthy empty inventories',async({page})=>{
  await fixture(page,{fail:true});await page.goto('/');
  await expect(page.locator('#notice')).toContainText('FLM is unavailable');
  await expect(page.getByRole('heading',{name:'Unable to load data'})).toBeVisible();
  await expect(page.getByRole('heading',{name:'No applications yet'})).toHaveCount(0);
});
test('invalid resource hashes normalize to applications',async({page})=>{
  await fixture(page);await page.goto('/#/settings');
  await expect(page).toHaveURL(/#\/applications$/);
  await expect(page.getByRole('button',{name:/flmping/})).toBeVisible();
});

test('resource navigation remains usable on a narrow screen',async({page})=>{
  await page.setViewportSize({width:390,height:844});await fixture(page);await page.goto('/');
  await page.getByRole('link',{name:'Executors',exact:true}).click();
  await expect(page.getByRole('heading',{name:'Executors',exact:true,level:1})).toBeVisible();
  await expect(page.getByRole('button',{name:'executor-01',exact:true})).toBeVisible();
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
});
