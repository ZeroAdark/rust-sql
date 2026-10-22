#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2024 Alexis Engelke
# SPDX-License-Identifier: BSD-3-Clause

import argparse
import json
from pathlib import Path
import re
import runpy
import struct

import encoder_domains as base
import source_model as source

SCALARS = {**base.SCALAR_TYPES, 'int32_t':'i32', 'int64_t':'i64', 'uintptr_t':'usize', 'ptrdiff_t':'isize', 'float':'f32', 'double':'f64'}
MASK = (1 << 64) - 1


def domain(family, options, ty, part):
    if ty in base.REGISTER_TYPES:
        return list(range(32))
    if ty == 'float':
        values = [0, 0x80000000, 0x3f800000, 0xbf800000, 0x7f800000, 0xff800000, 0x7fc00001, 1, 0xffffffff]
        values += [((i & 0x80) << 24) | (0x3e000000 if i & 0x40 else 0x40000000) | ((i & 0x3f) << 19) for i in range(256)]
    elif ty == 'double':
        values = [0, 1 << 63, 0x3ff0000000000000, 0xbff0000000000000, 0x7ff0000000000000, 0xfff0000000000000, 0x7ff8000000000001, 1, MASK]
        values += [((i & 0x80) << 56) | (0x3fc0000000000000 if i & 0x40 else 0x4000000000000000) | ((i & 0x3f) << 48) for i in range(256)]
    elif ty in ['int32_t', 'int64_t', 'ptrdiff_t']:
        values = [0,1,-1,2,-2,7,-7,8,-8,4095,4096,4097,-4095,-4096,-4097,(1 << 31)-1,1 << 31,-(1 << 31),-(1 << 31)-1,(1 << 63)-1,-(1 << 63),MASK]
        if family in ['simm','reladdr','immldraut']:
            bits = 13 if family == 'immldraut' else int(options['bits'],0)
            shift = 3 if family == 'immldraut' else int(options.get('asr','0'),0)
            width = bits if family == 'immldraut' else bits + shift
            high,low,quantum = (1 << (width-1))-1, -(1 << (width-1)), 1 << shift
            aligned = high // quantum * quantum
            values += [low-quantum,low-1,low,low+1,low+quantum,high-quantum,high-1,high,high+1,high+quantum,aligned,aligned+quantum,quantum-1,quantum,quantum+1,-quantum-1,-quantum,-quantum+1]
    elif ty == 'uintptr_t':
        values = [0,1,0xfff,0x1000,0x1001,0x1fffff,0x200000,(1 << 32)-1,1 << 32,(1 << 48)-1,1 << 48,(1 << 63)-1,1 << 63,MASK-0xfff,MASK]
    else:
        values = base.domain(family,options,ty,part)
    if family == 'immadd':
        values += [0xfff000,0xfff001,0x1000000,0x1000,0x1001,(-0xfff000)&MASK,(-0x1000)&MASK]
    if family == 'immlogical':
        width = int(options['size'],0)
        for size in [2,4,8,16,32,64]:
            if size > width:
                continue
            for length in range(1,size):
                for rotate in [0,1,size-1]:
                    elem = (1 << length)-1
                    elem = ((elem >> rotate) | (elem << (size-rotate))) & ((1 << size)-1)
                    values.append(sum(elem << bit for bit in range(0,width,size)))
    if family == 'immsimd8movi':
        values += [sum(0xff << (j*8) for j in range(8) if i & (1 << j)) for i in range(256)]
    return base.unique(value & MASK for value in values)


def parameters(form, functions):
    result,groups = [],[]
    for operand in form['operands'].split():
        variables,spec = operand.split('=',1)
        if spec.startswith('@'):
            continue
        family,_,option_text = spec.partition('/')
        options = dict(option.split('=',1) for option in option_text.split(',') if option)
        start = len(result)
        for part,parameter in enumerate(functions[family].paramtys):
            ty,_,name = parameter.partition('  ')
            ty = ty.format(**options)
            assert ty in base.REGISTER_TYPES or ty in SCALARS, (form['name'],family,ty)
            result.append({'name':name or variables.replace(',',''),'type':ty,'family':family,'options':options,'part':part,'domain':domain(family,options,ty,part)})
        if start != len(result):
            groups.append((family,options,list(range(start,len(result)))))
    return result,groups


def cases(params, groups, seed):
    result = dict(base.cases(params,groups,seed))
    baseline = list(next(iter(result)))
    for family,_,indices in groups:
        if family not in ['immadr','immadrp']:
            continue
        limit = 1 << (32 if family == 'immadrp' else 20)
        for origin in [0,0xfff,0x1000,0x1001,0x100001001,MASK-0xfff]:
            for delta in [-limit-4096,-limit-1,-limit,-limit+1,-4097,-4096,-1,0,1,4095,4096,4097,limit-1,limit,limit+1,limit+4096]:
                values = baseline.copy()
                values[indices[0]],values[indices[1]] = origin,(origin+delta)&MASK
                result.setdefault(tuple(values),f'address:{family}:{origin:#x}:{delta}')
    return result.items()


def argument(param,index):
    ty = param['type']
    if ty in base.REGISTER_TYPES:
        return f'{ty}{{static_cast<uint8_t>(args[{index}] & 31)}}'
    if ty == 'float':
        return f'input_f32(args[{index}])'
    if ty == 'double':
        return f'input_f64(args[{index}])'
    return f'static_cast<{ty}>(args[{index}])'


def encoders(output,model,profile):
    functions = runpy.run_path(str(source.REFERENCE/'parse.py'),run_name='__runtime_cases__')['ENC_FUNCS']
    forms = {form['name']:form for form in model['encoder_forms'] if form['enabled']}
    selected = {}
    for definition in model['encoder_declarations']:
        if not definition.startswith('static inline uint32_t '):
            continue
        match = base.DEFINITION.fullmatch(definition)
        assert match,definition
        name = match[2]
        params,groups = parameters(forms[name],functions)
        signature = ', '.join(f"{param['type']} {param['name']}" for param in params) or 'void'
        assert signature == match[3],(name,signature,match[3])
        assert name not in selected
        selected[name] = (params,groups,sorted(set(re.findall(r'\bda_\w+(?=\s*\()',match[4]))))
    assert len(selected) >= 3000, len(selected)
    if profile == "really-all":
        assert len(selected) == 3911, len(selected)
    count,helper_forms = 0,0
    arms,roster = [],[]
    with (output/'encoder-input.bin').open('wb') as binary, (output/'encoder-cases.tsv').open('w') as labels:
        labels.write('case\tencoder\tname\tlabel\toperands\n')
        for encoder_id,(name,(params,groups,helpers)) in enumerate(sorted(selected.items())):
            assert len(params) <= 8
            helper_forms += bool(helpers)
            arms.append(f"case {encoder_id}: value = de64_{name}({', '.join(argument(param,index) for index,param in enumerate(params))}); break;")
            first = count
            for values,label in cases(params,groups,0x13579bdf^(encoder_id+1)):
                padded = list(values)+[0]*(8-len(values))
                binary.write(struct.pack('<I8Q',encoder_id,*padded))
                labels.write(f'{count}\t{encoder_id}\t{name}\t{label}\t'+','.join(map(str,values))+'\n')
                count += 1
            assert count > first
            roster.append(f"{encoder_id}\t{name}\t{forms[name]['group']}\t{first}\t{count-first}\t{','.join(helpers)}\n")
    (output/'encoder-names.txt').write_text(''.join(name+'\n' for name in sorted(selected)))
    (output/'encoder-roster.tsv').write_text('encoder\tname\tgroup\tfirst_case\tcases\thelpers\n'+''.join(roster))
    (output/'encoder-omitted.tsv').write_text('name\tdisposition\n'+''.join(name+'\tUPSTREAM_OMITTED\n' for name in sorted(forms.keys()-selected.keys())))
    (output/'encoder-counts.tsv').write_text(f'forms\t{len(selected)}\nhelper_forms\t{helper_forms}\ncases\t{count}\nrecord_bytes\t68\nomitted\t{len(forms)-len(selected)}\n')
    assert helper_forms >= 150,helper_forms
    if profile == "really-all":
        assert helper_forms == 217,helper_forms
    template = (Path(__file__).parent/'encoder_reference.cpp.in').read_text()
    (output/'encoder_reference.cpp').write_text(template.replace('@ARMS@','\n'.join(arms)))


def decoder(output,model):
    state,count = 0x13579bdf,0
    patterns = model['trie_descriptions']
    with (output/'decoder-input.bin').open('wb') as binary, (output/'decoder-roster.tsv').open('w') as roster:
        roster.write('pattern\tname\tvalue\tmask\tfirst_case\tcases\n')
        def write(inst):
            nonlocal count
            seed = [0,0xfedcba9876543210,MASK][count%3]
            addr = [0,0x100001000,MASK-3,1 << 63][count%4]
            binary.write(struct.pack('<IQQ',inst,seed,addr))
            count += 1
        for index,((value,mask),mnem,name,condition) in enumerate(patterns):
            assert condition is None
            free = mask ^ 0xffffffff
            words = [value,value|free]
            for bit in range(32):
                if free & (1 << bit):
                    words += [value|(1 << bit),value|(free^(1 << bit))]
                else:
                    words += [value^(1 << bit)]
            for _ in range(8):
                state ^= (state << 13)&0xffffffff
                state ^= state >> 17
                state ^= (state << 5)&0xffffffff
                words.append(value|(state&free))
            first = count
            for word in words:
                write(word)
            roster.write(f'{index}\t{name}\t{value:08x}\t{mask:08x}\t{first}\t{count-first}\n')
        first_random = count
        for _ in range(262144):
            state ^= (state << 13)&0xffffffff
            state ^= state >> 17
            state ^= (state << 5)&0xffffffff
            write(state)
        for word in [0,0xffffffff,0xd503201f,0xd65f03c0]:
            write(word)
    (output/'decoder-counts.tsv').write_text(f'patterns\t{len(patterns)}\npattern_cases\t{first_random}\nrandom_cases\t262144\ncases\t{count}\nrecord_bytes\t20\n')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('output',type=Path)
    parser.add_argument('--profile',choices=['all','really-all'],required=True)
    parser.add_argument('--suite',choices=['helpers','encoders','decode'],required=True)
    args = parser.parse_args()
    source.verify_sources()
    source.generate(argparse.Namespace(output=args.output,features=args.profile,coverage=False,encode_in_header=True,no_feature_desc=False,feature_desc=None,desc=None,model=True,classifications=False))
    for name in ['classify.c','decode.c','encode.c','format.c','disarm64.h']:
        (args.output/name).write_bytes((source.REFERENCE/name).read_bytes())
    for original,target in [('public.inc','disarm64-public.inc'),('private.inc','disarm64-private.inc')]:
        (args.output/target).write_bytes((args.output/original).read_bytes())
    model = json.loads((args.output/'model.json').read_text())
    if args.suite == 'encoders':
        encoders(args.output,model,args.profile)
    elif args.suite == 'decode':
        decoder(args.output,model)
    (args.output/'generation-profile.txt').write_text(args.profile+'\n')
    print('TPDE_DISARM_RUNTIME_CASES_READY suite='+args.suite+' profile='+args.profile)


if __name__ == '__main__':
    main()
