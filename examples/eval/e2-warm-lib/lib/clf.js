// lib/clf.js —— 工单逐项判别库（V0-B 级联探针 e2-glm r2 自发产出 clf_llm.js 的蒸馏）
// io: clfOne(ticket {title, body}) -> label（llm 单次判别 + labels 校验 + clfParse 兜底）
//     clfVerify() -> {ok, total, bad}（链路健康探针：小样例内嵌、显式运行，任一错例 throw）
// 适用: 客服工单 7 类逐项判别；prompt 含类定义与 account↔bug 边界口径（协议 v2.2 同源）
// 来源: v0b_eval_e2_glm_inner/e2-r2 wd/lib/clf_llm.js（模型自发）+ 评测侧补 clfOne/clfVerify
// owner: v0b 评测侧（负责人批准 = 本行 st 与 INDEX 登记）
const CLF_LABELS = ["bug","feature","account","billing","howto","complaint","unsure"];
function clfSys(){ return "你是客服工单分类助手。直接给出结论，不要长篇分析。最后只输出一行标签词。"; }
function clfPrompt(t){
return "把下面这条客服工单归入 7 个标签之一：bug / feature / account / billing / howto / complaint / unsure\n\n"
+ "定义：\n"
+ "- bug：产品故障。报错、闪退、卡死、加载不出来、白屏、乱码、页面卡住/点不动/按钮失效、内容显示不全、数据丢失或不同步、上传导出出错、功能用不了。\n"
+ "- feature：新增或改进功能的建议/需求。希望支持X、建议增加X、什么时候能支持X、能不能加X、要是能X就好了、把X排上日程、想加某功能。\n"
+ "- account：账号问题。登录不了、验证码收不到、密码与找回、注册与激活邮件、绑定或换绑手机号/邮箱、账号被冻结/锁定/异地登录/掉线/解封、昵称、授权失败、账号认证。\n"
+ "- billing：费用与订单。扣费/扣款、退款、充值未到账、账单、发票/开票/凭证、价格不一致、多收钱、续费/自动续费、会员权益未到账、金额对不上。\n"
+ "- howto：使用方法咨询。怎么操作、功能在哪里、入口在哪、具体步骤、是否支持某功能（提问）、从哪进。\n"
+ "- complaint：不满与投诉。投诉、客服或流程态度差、要求主管/负责人出面、要说法、威胁曝光、多次催办仍无下文表达强烈不满。\n"
+ "- unsure：无法归入以上单一类别。两种情形：(1) 没有可识别的实质诉求（纯催促/问候/乱码/测试文本，如'急急急''在线等''测试测试''求处理'）；(2) 同时包含两个及以上属于不同类别的诉求/问题。\n\n"
+ "规则：\n"
+ "1) 忽略背景噪音：设备信息、版本号、网络环境、'已按FAQ核对''卸载重装仍复现''同事都没事''催办加急'等凑数句，不决定类别。\n"
+ "2) 找出真正诉求句；若属同一类别→输出该类别。\n"
+ "3) 若存在两个及以上不同类别的实质诉求→unsure。\n"
+ "4) 若没有可识别的实质诉求→unsure。\n"
+ "5) 边界口径：按根因域归类——登录、验证码、账号状态、绑定与认证类问题，即便被描述成\u201c进不去/被踢出/界面出不来\u201d等故障措辞也归 account；bug 只指账号流程之外的产品功能故障。\n\n"
+ "最后只输出一行标签词（只能是 bug/feature/account/billing/howto/complaint/unsure 之一）。\n\n"
+ "标题：" + t.title + "\n内容：" + t.body;
}
function clfParse(text){
  if(!text) return null;
  const s=String(text).replace(/[*`#>]/g," ");
  let best=null,bi=-1;
  for(const w of CLF_LABELS){ const i=s.lastIndexOf(w); if(i>bi){bi=i;best=w;} }
  return best;
}
function clfOne(t){
  const r = tools.call("llm", {prompt: clfPrompt(t), system: clfSys(),
                               labels: CLF_LABELS, max_tokens: 1024});
  return r.label || clfParse(r.text);
}
function clfVerify(){
  const cases = [
    {t:{title:"登不上", body:"验证码等了十分钟没收到，换手机也不行"}, want:"account"},
    {t:{title:"重复扣款", body:"同一笔订单扣了两次钱，多扣的请退回"}, want:"billing"},
    {t:{title:"闪退", body:"一打开报表页面就闪退，重装了也一样"}, want:"bug"},
    {t:{title:"建议", body:"能不能增加按周导出工时表的功能"}, want:"feature"},
    {t:{title:"入口在哪", body:"请问共享日历的权限设置从哪里进"}, want:"howto"},
    {t:{title:"投诉", body:"客服来回踢皮球三天了，要求主管出面给说法"}, want:"complaint"},
    {t:{title:"在吗", body:"急急急，在线等"}, want:"unsure"}
  ];
  const bad = [];
  for (const c of cases){
    const got = clfOne(c.t);
    if (got !== c.want) bad.push({title: c.t.title, want: c.want, got: got});
  }
  if (bad.length) throw new Error("clfVerify: " + JSON.stringify(bad));
  return {ok: true, total: cases.length, bad: []};
}
(function selfTest(){
  if(clfParse("这属于 billing 类")!=="billing") throw new Error("clfParse t1");
  if(clfParse("unsure，因为混合")!=="unsure") throw new Error("clfParse t2");
  if(clfParse("")!==null) throw new Error("clfParse t3");
  if(CLF_LABELS.length!==7) throw new Error("labels");
  const p=clfPrompt({title:"t",body:"b"});
  if(p.indexOf("unsure")<0||p.indexOf("内容：b")<0) throw new Error("prompt");
  if(p.indexOf("边界口径")<0) throw new Error("prompt boundary");
})();
