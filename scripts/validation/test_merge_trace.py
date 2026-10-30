from itertools import permutations
from pathlib import Path
import unittest

from compare_regression import compare,find_anchor,physical_lines,statements
from diagnostic_policy import guard_change
from merge_trace import compare_merge_trace,digest,policy,trace_block,valid_trace


ROOT=Path(__file__).resolve().parents[2]/"crates/postgres-18.6-reference/src/test/regress"


class MergeTraceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source=(ROOT/'sql/merge.sql').read_text();cls.expected=(ROOT/'expected/merge.out').read_text()
        cls.lines=physical_lines(cls.expected,keepends=True);plain=physical_lines(cls.expected);pos=0
        for line,anchor,sql in statements(cls.source):
            start=find_anchor(plain,anchor,pos);pos=start+len(anchor)
            if line==667:
                cls.start=pos;break
        cls.end,cls.header,cls.rows=trace_block(cls.lines,cls.start,policy())
        cls.block=cls.lines[cls.start:cls.end]

    def actual(self,block):
        return ''.join(self.lines[:self.start]+block+self.lines[self.end:])

    def test_source_and_input_oracle_match_real_fixture(self):
        self.assertEqual(digest(self.source),policy()['source_sha256'])
        self.assertEqual(digest(self.expected),policy()['expected_sha256'])
        self.assertEqual({line:sql for line,_,sql in statements(self.source)}[667],policy()['sql'])
        self.assertEqual(self.end-self.start,21)
        self.assertEqual(set(self.rows),{('UPDATE','(3,30)','(3,10)',3,10),('INSERT',None,'(4,40)',4,40),
                                        ('DELETE','(2,20)',None,2,20),('UPDATE','(1,10)','(1,0)',1,0)})

    def test_all_24_consistent_processing_orders(self):
        b=self.block
        for order in permutations(range(4)):
            block=b[:3]+[b[3+i] for i in order]+[b[7+i] for i in order]+b[11:16]+[b[16+i] for i in order]+b[20:]
            actual=self.actual(block);normalized,proofs=compare_merge_trace(self.source,self.expected,actual)
            self.assertEqual(normalized,self.expected)
            self.assertEqual(len(proofs),int(actual!=self.expected))

    def test_notices_cannot_be_sorted_independently(self):
        for offset in (0,3,7,11,16):
            block=self.block.copy();block[offset],block[offset+1]=block[offset+1],block[offset]
            actual=self.actual(block)
            self.assertEqual(compare_merge_trace(self.source,self.expected,actual),(actual,[]))

    def test_changed_actions_values_missing_or_duplicate_rows_fail(self):
        blocks=[]
        for old,new in [('(3,30)','(3,31)'),('(3,10)','(3,11)'),('UPDATE','DELETE'),('  3 |','  9 |')]:
            blocks.append([line.replace(old,new) for line in self.block])
        blocks += [self.block[:3]+self.block[4:],self.block[:16]+[self.block[16]]+self.block[16:],
                   self.block[:16]+[self.block[17]]+self.block[17:]]
        for block in blocks:
            actual=self.actual(block)
            self.assertEqual(compare_merge_trace(self.source,self.expected,actual),(actual,[]))

    def test_stale_source_labels_line_endings_and_external_errors_fail(self):
        reordered=self.block[:3]+[self.block[4],self.block[3],*self.block[5:7]]+[self.block[8],self.block[7],*self.block[9:11]]+self.block[11:16]+[self.block[17],self.block[16],*self.block[18:]]
        actual=self.actual(reordered)
        for source,expected in ((self.source+'-- drift\n',self.expected),(self.source,self.expected+'\n')):
            self.assertEqual(compare_merge_trace(source,expected,actual),(actual,[]))
        for altered in (actual.replace(' merge_action |',' wrong_action |',1),actual.replace('NOTICE:  BEFORE UPDATE ROW trigger row: (3,30) -> (3,10)\n','NOTICE:  BEFORE UPDATE ROW trigger row: (3,30) -> (3,10)\r\n')):
            self.assertNotEqual(compare_merge_trace(self.source,self.expected,altered)[0],self.expected)
        altered=actual+'ERROR: unrelated failure\n'
        normalized,_=compare_merge_trace(self.source,self.expected,altered)
        self.assertFalse(compare(self.source,self.expected,normalized)[0]['equal_after_permitted_order'])

    def test_merge_action_counters_cannot_change_or_disappear(self):
        root=' Merge on t (actual rows=0.00 loops=1)';count='   Tuples: updated=5 deleted=5 skipped=40'
        rule={'kind':'merge_plan','header':'explain_merge'}
        guard_change([root,count],[root,count],rule);guard_change([root],[root],rule)
        for after in ([root],[root,count.replace('updated=5','updated=4')],[root,count,count],[root.replace('loops=1','loops=2'),count]):
            with self.assertRaises(ValueError):guard_change([root,count],after,rule)


if __name__=='__main__':unittest.main()
