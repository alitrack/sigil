import copy
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location('dream_core', Path(__file__).with_name('dream_core.py'))
core = importlib.util.module_from_spec(SPEC) if SPEC else None
if SPEC and Path(SPEC.origin).exists():
    SPEC.loader.exec_module(core)


def node(id, parent, score, **extra):
    return dict(id=id, parent=parent, score=score, resumable=True, diagnostics=[], **extra)


def world(nodes=None):
    return {'version': 1, 'root': 'root', 'compatibility': {'task_family': 'json-edit', 'evaluator_version': 1, 'model': 'fake', 'binary_sha256': 'a'*64}, 'nodes': nodes or [node('root', None, 0), node('a','root',.2),node('b','root',.4),node('a1','a',.8),node('b1','b',.6)]}


def policy(**kw):
    return dict(version=1,name='baseline',branch_width=2,ranking='score',stagnation=0,stop_score=None,max_attempts=8,**{}) | kw


class DreamTests(unittest.TestCase):
    def test_api_exists(self):
        self.assertTrue(hasattr(core, 'validate_policy'), 'DREAM core API not implemented')

    def test_validation(self):
        self.assertEqual(core.validate_policy(policy()), policy())
        for key, values in {'version':[True,2], 'name':['../x',''], 'branch_width':[True,0,33], 'ranking':['random'], 'stagnation':[-1,65], 'stop_score':[True,float('nan'),float('inf'),1.1], 'max_attempts':[True,0,257]}.items():
            for value in values:
                with self.subTest(key=key,value=value), self.assertRaises(ValueError): core.validate_policy(policy(**{key:value}))
        for mutate in [lambda w:w['nodes'].append(node('a','root',0)),lambda w:w['nodes'].__setitem__(1,node('a','missing',0)),lambda w:w['nodes'].__setitem__(1,node('a','a',0)),lambda w:w['nodes'].__setitem__(1,node('a','root',True)),lambda w:w['nodes'].__setitem__(1,node('a','root',float('nan'))),lambda w:w['nodes'].__setitem__(1,node('a','root',0,snapshot='../secret')),lambda w:w['nodes'].append(node('a2','a',0))]:
            w=world(); mutate(w)
            with self.assertRaisesRegex(ValueError,'Invalid'): core.validate_world(w)
        w=world(); w['nodes'][1]['snapshot']='nodes/a/snapshot'
        self.assertEqual(core.validate_world(w),w)

    def test_root_order_and_support_exhaustion(self):
        r=core.replay(world(),policy(),workers=1)
        self.assertEqual(r['batches'][:2],[['root'],['root']])
        self.assertEqual(r['reached_nodes'],['root','a','b','b1','a1'])
        self.assertEqual(r['stop_reason'],'support_exhausted')
        self.assertAlmostEqual(r['objective'],.76)

    def test_batch_arithmetic_and_caps(self):
        r=core.replay(world(),policy(),workers=2,beta1=.1,beta2=.2)
        self.assertEqual(r['batches'],[['root'],['root','a'],['b']])
        self.assertEqual(r['represented_attempts'],4)
        self.assertEqual(r['decision_rounds'],3)
        self.assertAlmostEqual(r['objective'],.8-.4+.8/3)
        r=core.replay(world(),policy(max_attempts=3),workers=32)
        self.assertEqual(r['represented_attempts'],3)
        self.assertEqual(r['stop_reason'],'attempts_limit')
        self.assertEqual(core.replay(world(),policy(),max_rounds=1)['stop_reason'],'rounds_limit')

    def test_hidden_scores_and_legal_leaves(self):
        seen=world()['nodes'][:3]
        self.assertEqual(core.select_batch(policy(),seen,['root','a','b'],2),['b','a'])
        changed=world(); changed['nodes'][-1]['score']=1
        self.assertEqual(core.replay(world(),policy(),max_rounds=2)['batches'],core.replay(changed,policy(),max_rounds=2)['batches'])
        seen.append(node('a1','a',.8))
        self.assertNotIn('a',core.select_batch(policy(),seen,['root','a','b','a1'],32))
        seen[-1]['resumable']=False
        self.assertNotIn('a1',core.select_batch(policy(),seen,['a1'],1))

    def test_ranking_stagnation_stop(self):
        seen=[node('root',None,0),node('a','root',.5),node('a1','a',.4),node('b','root',.3)]
        self.assertEqual(core.select_batch(policy(ranking='depth'),seen,['a1','b'],1),['a1'])
        self.assertEqual(core.select_batch(policy(ranking='breadth'),seen,['a1','b'],1),['b'])
        self.assertEqual(core.select_batch(policy(stagnation=1),seen,['a1','b'],2),['b'])
        r=core.replay(world(),policy(stop_score=0))
        self.assertEqual(r['represented_attempts'],0)
        self.assertEqual(r['stop_reason'],'policy_stop')

    def test_selection_and_candidates(self):
        incumbent=policy(stop_score=0)
        selected=core.select_policy([world(),world()],incumbent,[policy(),policy(name='invalid',branch_width=0)])
        self.assertEqual(selected['selected_policy'],policy())
        self.assertFalse(selected['comparisons'][-1]['eligible'])
        ties=core.select_policy([world()],policy(),[policy(name='tie')])
        self.assertEqual(ties['selected_policy'],policy())
        worse=core.select_policy([world()],policy(),[incumbent])
        self.assertEqual(worse['selected_policy'],policy())
        incompatible=world(); incompatible['compatibility']['model']='other'
        with self.assertRaises(ValueError): core.select_policy([world(),incompatible],policy(),[])
        candidates=core.candidate_policies(policy())
        self.assertIn(policy(),candidates)
        self.assertLessEqual(len(candidates),37)
        for candidate in candidates: core.validate_policy(candidate)

    def test_malformed_bounds_are_safe(self):
        for value in [10**1000, [], {}, 'secret']:
            with self.assertRaisesRegex(ValueError, '^Invalid policy$'):
                core.validate_policy(policy(stop_score=value))
        w=world(); w['nodes'][1]['score']=10**1000
        with self.assertRaisesRegex(ValueError, '^Invalid world$'): core.validate_world(w)
        for ref in ['/absolute','a//b','a/./b','a\\b','']:
            w=world(); w['nodes'][1]['session']=ref
            with self.assertRaises(ValueError): core.validate_world(w)
        w=world(); w['nodes'][1]['diagnostics']=['x'*4097]
        with self.assertRaises(ValueError): core.validate_world(w)
        w=world(); w['nodes']=[node('root',None,0)]+[node(f'n{i}','root',0) for i in range(4096)]
        with self.assertRaises(ValueError): core.validate_world(w)

    def test_optional_frozen_compatibility(self):
        w=world(); w['compatibility'].update(base_url='http://localhost:3001/v1',harness_sha256='b'*64)
        self.assertEqual(core.validate_world(w),w)
        other=copy.deepcopy(w); other['compatibility']['harness_sha256']='c'*64
        with self.assertRaises(ValueError): core.select_policy([w,other],policy(),[])
        for key,value in [('base_url',True),('harness_sha256','invalid'),('unknown','x')]:
            bad=copy.deepcopy(w); bad['compatibility'][key]=value
            with self.assertRaises(ValueError): core.validate_world(bad)

    def test_windows_references_rejected(self):
        for ref in ['C:/outside','C:relative','nodes/a:stream']:
            w=world(); w['nodes'][1]['snapshot']=ref
            with self.assertRaises(ValueError): core.validate_world(w)

    def test_terminal_parents_reject_lineage(self):
        for ident in ['root','a']:
            w=world()
            next(n for n in w['nodes'] if n['id']==ident)['resumable']=False
            with self.assertRaises(ValueError): core.validate_world(w)

    def test_shared_termination(self):
        seen=world()['nodes'][:2]
        self.assertIsNone(core.termination_reason(policy(),seen,1,64))
        self.assertEqual(core.termination_reason(policy(stop_score=.2),seen,1,64),'policy_stop')
        self.assertEqual(core.termination_reason(policy(max_attempts=1),seen,1,64),'attempts_limit')
        self.assertEqual(core.termination_reason(policy(),seen,1,1),'rounds_limit')
        self.assertEqual(core.termination_reason(policy(stop_score=.2,max_attempts=1),seen,1,1),'policy_stop')
        for rounds,limit in [(True,64),(-1,64),(1,0),(1,257)]:
            with self.assertRaises(ValueError): core.termination_reason(policy(),seen,rounds,limit)

    def test_world_sources_do_not_mix(self):
        online=world(); online['source']='online-discovery'
        simulated=world(); simulated['source']='simulated'
        for w in [online,simulated]: self.assertEqual(core.validate_world(w),w)
        for sources in [[online,simulated],[online,world()],[simulated,world()]]:
            with self.assertRaises(ValueError): core.select_policy(sources,policy(),[])
        self.assertEqual(core.select_policy([simulated,simulated],policy(),[])['selected_policy'],policy())
        for source in [None,True,'unknown',[]]:
            bad=world();bad['source']=source
            with self.assertRaises(ValueError): core.validate_world(bad)

    def test_policy_quota_with_terminal_support_reports_exhaustion(self):
        w=world([node('root',None,0),node('a','root',1),node('b','root',1)])
        r=core.replay(w,policy(branch_width=1))
        self.assertEqual(r['reached_nodes'],['root','a'])
        self.assertEqual(r['stop_reason'],'support_exhausted')
        self.assertAlmostEqual(r['objective'],.99)
        self.assertEqual(core.replay(w,policy(branch_width=1,stop_score=1))['stop_reason'],'policy_stop')

    def test_stagnation_stop_when_support_is_empty(self):
        w=world([node('root',None,0),node('a','root',.5),node('a1','a',.4)])
        self.assertEqual(core.replay(w,policy(branch_width=1,stagnation=1))['stop_reason'],'policy_stop')
        self.assertEqual(core.replay(w,policy(branch_width=1))['stop_reason'],'support_exhausted')

    def test_replay_bounds(self):
        for args in [dict(workers=True),dict(workers=33),dict(max_rounds=0),dict(beta1=float('nan')),dict(beta2=-1)]:
            with self.assertRaises(ValueError): core.replay(world(),policy(),**args)

if __name__ == '__main__': unittest.main()
