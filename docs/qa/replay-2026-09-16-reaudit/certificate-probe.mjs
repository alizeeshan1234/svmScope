import fs from 'node:fs';
import vm from 'node:vm';
const html=fs.readFileSync(new URL('../../../static/index.html', import.meta.url),'utf8');
const begin=html.indexOf('function renderCertificate(d) {');
const end=html.indexOf('// ---------------- one-click what-if scenarios',begin);
const context=vm.createContext({analysis:null,fmt:String,esc:String,addrList:a=>a.join(', '),slotsAway:String});
vm.runInContext(html.slice(begin,end),context);
const base={slot:10,landed_slot:10,clock:'slot 10',result:{success:true,compute_units:999},onchain_success:true};
const cases=[
 ['different_errors_same_failure',{...base,result:{success:false,error:'InstructionError(0, Custom(6001))',compute_units:1},onchain_success:false,drifted:[],accounts:[],sources:{}}],
 ['unproven_balance_recorded',{...base,drifted:['pool'],accounts:[{address:'pool',source:{Recorded:{slot:9}}}],sources:{Recorded:1}}],
 ['unproven_absence',{...base,drifted:['missing'],accounts:[{address:'missing',source:{Absent:{proven:false}}}],sources:{Absent:1}}],
 ['partial_event_fields',{...base,drifted:[],accounts:[{address:'curve',source:{EventLog:{slot:9}}}],sources:{EventLog:1}}],
 ['unknown_program_version',{...base,drifted:[],accounts:[{address:'program',source:{Program:{upgraded_since:null}}}],sources:{Program:1}}],
];
const results=cases.map(([name,data])=>{context.input=data;const output=vm.runInContext('renderCertificate(input)',context);return {name,claims_exact:output.includes('this replay is exact'),claims_matches:output.includes('matches the real on-chain result'),text:output.replace(/<[^>]*>/g,' ').replace(/\s+/g,' ').trim()};});
console.log(JSON.stringify(results,null,2));
