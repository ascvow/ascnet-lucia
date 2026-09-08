import { expect, test } from "bun:test";
import { implementationRank, paired } from "./replay_real_projects";

/** 同文件与近邻行不能替代关键实现；未命中必须保留在分母。 */
test("重放评分要求关键范围与正文同时存在", () => {
  const target = [{path:"a.ts",start_line:10,end_line:12,needles:["save()"]}];
  expect(implementationRank([{path:"a.ts",start_line:1,end_line:9,content:"save()"}],target)).toBeNull();
  expect(implementationRank([{path:"a.ts",start_line:10,end_line:12,content:"load()"}],target)).toBeNull();
  expect(implementationRank([{path:"a.ts",start_line:10,end_line:12,content:"save()"}],target)).toBe(1);
  const result = paired([{before:null,after:1},{before:2,after:null},{before:null,after:null}]);
  expect(result.n).toBe(3);
  expect([result.wins,result.losses,result.ties]).toEqual([1,1,1]);
  expect(result.after.top10).toBe(1);
});
